//! V35 (#223 S11): a song's video upgrade bookkeeping, NULL for every row
//! before (never checked).

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

#[tokio::test]
async fn v35_adds_the_upgrade_columns_null_for_old_rows() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 34).await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (35001, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id) VALUES (35001, 35001, 'yt35aaaaaaa')",
    )
    .execute(&pool)
    .await
    .unwrap();
    apply_upto(&pool, 35).await;
    let row: (Option<i64>, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT video_upgrade_cap, video_upgrade_state, video_upgrade_at \
             FROM videos WHERE id = 35001",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, (None, None, None));
    sqlx::query(
        "UPDATE videos SET video_upgrade_cap = 2160, video_upgrade_state = 'upgraded', \
         video_upgrade_at = 1760000000000 WHERE id = 35001",
    )
    .execute(&pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn migration_v35_is_in_the_list() {
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 35, "{latest}");
}
