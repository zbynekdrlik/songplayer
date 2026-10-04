//! #225: playlist queries for the dashboard WebSocket (own module:
//! `db/models.rs` is at the 1000-line cap).

use sqlx::SqlitePool;

/// Every playlist id, ascending — active or not, since the dashboard lists
/// them all. The WS on-connect replay tells each one's state.
pub async fn all_playlist_ids(pool: &SqlitePool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM playlists ORDER BY id")
        .fetch_all(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_playlist_id_ascending_inactive_ones_too() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        let seeded = all_playlist_ids(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, is_active) \
             VALUES (22521, 'b', 'u-b', 0), (22520, 'a', 'u-a', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut expected = seeded;
        expected.extend([22_520, 22_521]);
        expected.sort_unstable();
        assert_eq!(all_playlist_ids(&pool).await.unwrap(), expected);
    }
}
