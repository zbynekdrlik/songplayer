//! Dabing section seed (#180 — dubbing D1). Split out of `startup.rs` to keep
//! it under the 1000-line airuleset cap; `#[path]`-included there and
//! re-exported, so the call site stays `startup::ensure_dabing_playlist_exists`.

use sqlx::SqlitePool;

/// Idempotently create the single `kind='dabing'` playlist that backs the
/// Dabing section — mirrors [`super::ensure_live_playlist_exists`] (the SP-live
/// seed). `ndi_output_name = 'SP-dabing'` (the section's own output; the OBS
/// scene `sp-dabing` is created by hand in D5, legacy `yt*` scenes untouched).
/// Runs on every startup; the `WHERE NOT EXISTS` guard makes re-runs no-ops.
pub async fn ensure_dabing_playlist_exists(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO playlists
            (name, youtube_url, ndi_output_name, playback_mode, is_active, kind)
         SELECT 'Dabing', '', 'SP-dabing', 'continuous', 1, 'dabing'
         WHERE NOT EXISTS (SELECT 1 FROM playlists WHERE kind = 'dabing')",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{create_memory_pool, run_migrations};

    async fn pool() -> SqlitePool {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn seeds_one_dabing_playlist_with_expected_fields() {
        let pool = pool().await;
        ensure_dabing_playlist_exists(&pool).await.unwrap();

        let row = sqlx::query(
            "SELECT name, kind, ndi_output_name, is_active FROM playlists WHERE kind = 'dabing'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        use sqlx::Row;
        assert_eq!(row.get::<String, _>("name"), "Dabing");
        assert_eq!(row.get::<String, _>("kind"), "dabing");
        assert_eq!(row.get::<String, _>("ndi_output_name"), "SP-dabing");
        assert_eq!(row.get::<i64, _>("is_active"), 1);
    }

    #[tokio::test]
    async fn is_idempotent() {
        let pool = pool().await;
        ensure_dabing_playlist_exists(&pool).await.unwrap();
        ensure_dabing_playlist_exists(&pool).await.unwrap();
        ensure_dabing_playlist_exists(&pool).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playlists WHERE kind = 'dabing'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "re-running the seed must not create duplicates");
    }
}
