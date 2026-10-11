//! V36 (#250): a video's lyrics register check, unchecked (0, NULL) for every
//! row before.

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

#[tokio::test]
async fn v36_adds_the_register_columns_unchecked_for_old_rows() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 35).await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (36001, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id) VALUES (36001, 36001, 'yt36aaaaaaa')",
    )
    .execute(&pool)
    .await
    .unwrap();
    apply_upto(&pool, 36).await;
    let row: (i64, Option<String>) = sqlx::query_as(
        "SELECT lyrics_register_version, lyrics_register_verdict FROM videos WHERE id = 36001",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, (0, None));
}

#[tokio::test]
async fn migration_v36_is_in_the_list() {
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 36, "{latest}");
}
