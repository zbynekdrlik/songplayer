//! Admin endpoint for one-shot catalog reprocess under the new source gate.
//!
//! `POST /api/v1/lyrics/reprocess-catalog-with-new-gate`:
//!   1. Restamps any `pipeline_version > 20` rows down to 19 (un-anomalies
//!      the 77 rows produced by a now-reverted code path that bumped the
//!      constant without approval).
//!   2. Clears stuck `lyrics_manual_priority = 1` on terminal-sentinel rows
//!      (`no_source` / `failed` / `empty`). Pre-fix versions of
//!      `db::models::mark_video_lyrics` left the flag dangling when gather
//!      returned zero candidates; the cleanup pass purges those.
//!   3. Sets `lyrics_manual_priority = 1` on every row whose
//!      `pipeline_version < current` AND `lyrics_source` is not a parked
//!      sentinel (`asr_gap` / `unsupported_source`).
//!   4. Returns `{ "restamped": N, "dangling_cleared": K, "queued": M }`.
//!
//! Idempotent — calling twice produces zeros on the second call (the WHERE
//! clauses no longer match after the first call).
//!
//! See `docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md`.

use crate::AppState;
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use tracing::warn;

#[derive(Debug, Serialize, PartialEq)]
pub struct ReprocessCatalogResponse {
    pub restamped: u64,
    pub dangling_cleared: u64,
    pub queued: u64,
}

#[cfg_attr(test, mutants::skip)] // Thin glue: three UPDATEs and a JSON
// response; observable side effects are covered by the six sibling tests
// below — five run the unit-level SQL through `run_endpoint_sql` (counts on
// each UPDATE, idempotency, dangling-flag cleanup, JSON shape) and the
// sixth (`endpoint_router_returns_correct_counts_and_clears_dangling`)
// invokes the real handler via `crate::api::router` + `tower::ServiceExt`
// to lock URL route, bind order, and JSON shape. Remaining mutation
// targets reduce to SQL string literals that cargo-mutants cannot mutate
// meaningfully.
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

    // Step 2: clear dangling manual_priority on terminal-sentinel rows.
    // Pre-fix `mark_video_lyrics` (`db/models.rs`) did not clear the manual flag
    // on the failure path, so 10 production rows ended up `lyrics_source =
    // 'no_source' AND lyrics_manual_priority = 1` after the initial reprocess.
    // They're parked by the skip-list either way, but the dangling flag confuses
    // queue-count queries. This cleanup is idempotent — second call matches 0.
    let dangling_cleared = match sqlx::query(
        "UPDATE videos SET lyrics_manual_priority = 0 \
         WHERE lyrics_manual_priority = 1 \
           AND lyrics_source IN ('failed', 'empty', 'no_source')",
    )
    .execute(&state.pool)
    .await
    {
        Ok(r) => r.rows_affected(),
        Err(e) => {
            warn!("reprocess_catalog_with_new_gate cleanup error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // Step 3: queue all candidate rows.
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
        dangling_cleared,
        queued,
        "reprocess-catalog-with-new-gate complete"
    );

    Json(ReprocessCatalogResponse {
        restamped,
        dangling_cleared,
        queued,
    })
    .into_response()
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

    /// Replicate the endpoint's three UPDATEs against an in-memory pool so we
    /// can assert counts without spinning up an Axum router. The `endpoint_*`
    /// tests below cover unit-level SQL behavior; `endpoint_router_returns_*`
    /// (further down) goes through the real `crate::api::router` to lock the
    /// handler + JSON shape + URL route + bind-order contract.
    ///
    /// IMPORTANT: keep this SQL byte-for-byte identical to the handler's SQL
    /// at `reprocess_catalog_with_new_gate`. Any change to the handler must
    /// be reflected here. Drift detection is the router test's job.
    async fn run_endpoint_sql(pool: &SqlitePool) -> ReprocessCatalogResponse {
        use crate::lyrics::LYRICS_PIPELINE_VERSION;

        let restamped = sqlx::query(
            "UPDATE videos SET lyrics_pipeline_version = 19 \
             WHERE lyrics_pipeline_version > 20",
        )
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();

        let dangling_cleared = sqlx::query(
            "UPDATE videos SET lyrics_manual_priority = 0 \
             WHERE lyrics_manual_priority = 1 \
               AND lyrics_source IN ('failed', 'empty', 'no_source')",
        )
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();

        let queued = sqlx::query(
            "UPDATE videos SET lyrics_manual_priority = 1 \
             WHERE lyrics_pipeline_version < ? \
               AND lyrics_manual_priority = 0 \
               AND (lyrics_source IS NULL \
                    OR lyrics_source NOT IN ('asr_gap', 'unsupported_source'))",
        )
        .bind(LYRICS_PIPELINE_VERSION as i64)
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();

        ReprocessCatalogResponse {
            restamped,
            dangling_cleared,
            queued,
        }
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
        //   - Cleanup: WHERE lyrics_manual_priority = 1 AND lyrics_source IN
        //     (terminal sentinels) → no rows match after the first call cleared
        //     them.
        //   - Queue: WHERE lyrics_pipeline_version < 20 AND lyrics_manual_priority = 0
        //     → no rows match after the first call set every eligible row's
        //     manual_priority to 1.
        // So all three counts must be 0 on the second call.
        assert_eq!(second.restamped, 0, "second restamp must affect 0 rows");
        assert_eq!(
            second.dangling_cleared, 0,
            "second cleanup must affect 0 rows"
        );
        assert_eq!(second.queued, 0, "second queue must affect 0 rows");
    }

    #[tokio::test]
    async fn endpoint_clears_dangling_manual_priority_on_terminal_sentinels() {
        // Regression for the production bug observed 2026-05-16: pre-fix
        // `db::models::mark_video_lyrics` left manual_priority=1 on rows that
        // gather failed for. 10 production rows ended up `lyrics_source =
        // 'no_source' AND lyrics_manual_priority = 1`. The admin endpoint
        // cleans those up.
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // 3 dangling rows: no_source/failed/empty all with manual_priority=1.
        // Each should be cleared.
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, \
                                 lyrics_pipeline_version, lyrics_manual_priority) VALUES \
             (1, 'a', 't', 'no_source', 20, 1), \
             (1, 'b', 't', 'failed', 20, 1), \
             (1, 'c', 't', 'empty', 20, 1), \
             (1, 'd', 't', 'asr_gap', 20, 1), \
             (1, 'e', 't', 'unsupported_source', 20, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let r = run_endpoint_sql(&pool).await;
        // Only the 3 terminal-sentinel rows (no_source/failed/empty) should be
        // cleared. asr_gap and unsupported_source are parked-but-not-terminal
        // for the purpose of this cleanup (they keep their manual_priority).
        assert_eq!(
            r.dangling_cleared, 3,
            "exactly 3 terminal-sentinel rows must be cleared"
        );
        // Verify the asr_gap and unsupported_source rows were NOT touched.
        let asr_gap_prio: i64 =
            sqlx::query_scalar("SELECT lyrics_manual_priority FROM videos WHERE youtube_id = 'd'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let unsup_prio: i64 =
            sqlx::query_scalar("SELECT lyrics_manual_priority FROM videos WHERE youtube_id = 'e'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            asr_gap_prio, 1,
            "asr_gap row's manual_priority must NOT be cleared by this step"
        );
        assert_eq!(
            unsup_prio, 1,
            "unsupported_source row's manual_priority must NOT be cleared by this step"
        );
    }

    #[tokio::test]
    async fn endpoint_response_serializes_to_expected_json_shape() {
        let r = ReprocessCatalogResponse {
            restamped: 7,
            dangling_cleared: 3,
            queued: 42,
        };
        let body = serde_json::to_string(&r).unwrap();
        assert_eq!(body, r#"{"restamped":7,"dangling_cleared":3,"queued":42}"#);
    }

    /// Router-driven test — invokes the real handler through `crate::api::router`
    /// + `tower::ServiceExt::oneshot`. Locks the URL route, the bind order in
    /// the production SQL (not the `run_endpoint_sql` mirror), and the JSON
    /// response shape. If the handler's SQL ever drifts from the mirror, this
    /// test catches it while the unit tests would silently pass.
    /// Build an `AppState` for tests that need to invoke handlers through the
    /// real `crate::api::router`. Inline mirror of the helper in
    /// `crate::api::lyrics::tests` — duplicated here because that helper isn't
    /// pub. Trade-off: a few lines of boilerplate vs. promoting a private test
    /// helper to crate-scope (which would expand the public test-only surface).
    async fn router_test_state() -> (crate::AppState, tempfile::TempDir) {
        use std::sync::Arc;
        use tokio::sync::{RwLock, broadcast, mpsc};
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_path_buf();
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        let (event_tx, _) = broadcast::channel(16);
        let (engine_tx, _) = mpsc::channel(16);
        let (sync_tx, _) = mpsc::channel(16);
        let (resolume_tx, _) = mpsc::channel(16);
        let (obs_rebuild_tx, _) = broadcast::channel(4);
        let state = crate::AppState {
            pool,
            event_tx,
            engine_tx,
            obs_state: Arc::new(RwLock::new(crate::obs::ObsState::default())),
            tools_status: Arc::new(RwLock::new(crate::ToolsStatus::default())),
            tool_paths: Arc::new(RwLock::new(None)),
            sync_tx,
            resolume_tx,
            obs_rebuild_tx,
            cache_dir: cache_dir.clone(),
            ai_proxy: Arc::new(crate::ai::proxy::ProxyManager::new(
                cache_dir,
                crate::ai::proxy::ProxyManager::default_port(),
            )),
            ai_client: Arc::new(crate::ai::client::AiClient::new(
                crate::ai::AiSettings::default(),
            )),
            presenter_client: None,
            resolume_registry: Arc::new(crate::resolume::ResolumeRegistry::new()),
            ndi_health_registry: Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        };
        (state, tmp)
    }

    #[tokio::test]
    async fn endpoint_router_returns_correct_counts_and_clears_dangling() {
        use axum::body::{self, Body};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (state, _tmp) = router_test_state().await;

        // Seed a mix:
        //   - v21 yt_subs row (will be restamped 21→19, then queued)
        //   - v15 lrclib row (will be queued)
        //   - v20 no_source row WITH manual_priority=1 (will be cleared)
        //   - v20 asr_gap row WITH manual_priority=1 (must NOT be cleared)
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)",
        )
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, \
                                 lyrics_pipeline_version, lyrics_manual_priority) VALUES \
             (1, 'r-v21', 't', 'yt_subs', 21, 0), \
             (1, 'r-v15', 't', 'lrclib', 15, 0), \
             (1, 'r-ns',  't', 'no_source', 20, 1), \
             (1, 'r-asr', 't', 'asr_gap', 20, 1)",
        )
        .execute(&state.pool)
        .await
        .unwrap();

        let app = crate::api::router(state.clone(), None);
        let req = Request::builder()
            .uri("/api/v1/lyrics/reprocess-catalog-with-new-gate")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: ReprocessCatalogResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            parsed,
            ReprocessCatalogResponse {
                restamped: 1,
                dangling_cleared: 1,
                queued: 2,
            },
            "router response must match the seeded counts exactly"
        );

        // Verify post-state in the DB:
        //   - r-v21 → version 19, manual_priority 1
        //   - r-v15 → manual_priority 1
        //   - r-ns  → manual_priority 0 (cleared)
        //   - r-asr → manual_priority 1 (still parked, unchanged)
        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT youtube_id, lyrics_pipeline_version, lyrics_manual_priority \
             FROM videos ORDER BY youtube_id",
        )
        .fetch_all(&state.pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("r-asr".into(), 20, 1),
                ("r-ns".into(), 20, 0),
                ("r-v15".into(), 15, 1),
                ("r-v21".into(), 19, 1),
            ]
        );
    }
}
