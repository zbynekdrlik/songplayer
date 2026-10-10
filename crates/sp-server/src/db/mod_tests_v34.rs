//! V34 (#223 S9b): a song's downloaded video format columns, NULL for every
//! row downloaded before.

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

#[tokio::test]
async fn v34_adds_the_video_format_columns_null_for_old_rows() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 33).await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (34001, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id) VALUES (34001, 34001, 'yt34aaaaaaa')",
    )
    .execute(&pool)
    .await
    .unwrap();
    apply_upto(&pool, 34).await;
    let row: (
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<f64>,
    ) = sqlx::query_as(
        "SELECT video_format_id, video_codec, video_width, video_height, video_fps \
             FROM videos WHERE id = 34001",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, (None, None, None, None, None));
    sqlx::query(
        "UPDATE videos SET video_format_id = '401', video_codec = 'av01.0.12M.08', \
         video_width = 3840, video_height = 2160, video_fps = 25.0 WHERE id = 34001",
    )
    .execute(&pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn migration_v34_is_in_the_list() {
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 34, "{latest}");
}
