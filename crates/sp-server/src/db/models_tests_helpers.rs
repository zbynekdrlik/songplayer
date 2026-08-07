//! Shared test fixtures for the `db::models` test suite, split out of the
//! former monolithic `models_tests.rs` (#137) so every sibling test file
//! (`models_tests_playlist.rs`, `models_tests_video.rs`,
//! `models_tests_lyrics.rs`) can import instead of duplicating. Referenced
//! from `models.rs` via `#[path = "models_tests_helpers.rs"] #[cfg(test)]
//! mod tests_helpers;`.

use super::*;
use crate::db;

/// Seed a single playlist + one unnormalized video row and return the pool
/// plus that video's id. Shared by every test that needs an existing video
/// row to mutate.
pub(crate) async fn setup_with_video() -> (SqlitePool, i64) {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Insert an unnormalized video row; the tests will mark it processed.
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized) VALUES (1, 'yt123', 't', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = sqlx::query("SELECT id FROM videos WHERE youtube_id = 'yt123'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let id: i64 = row.get("id");
    (pool, id)
}
