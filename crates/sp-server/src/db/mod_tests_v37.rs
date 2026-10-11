//! V37 (#229 follow-up): a playlist's remembered Loop song, NULL for every
//! row before.

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

#[tokio::test]
async fn v37_adds_the_loop_song_null_for_old_rows() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 36).await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (37001, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    apply_upto(&pool, 37).await;
    let loop_video: Option<i64> =
        sqlx::query_scalar("SELECT loop_video_id FROM playlists WHERE id = 37001")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(loop_video, None);
}

#[tokio::test]
async fn migration_v37_is_in_the_list() {
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 37, "{latest}");
}
