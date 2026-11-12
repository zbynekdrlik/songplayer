//! V44 (#265): the node's song block list.

use super::test_helpers::{apply_first_n, apply_upto, column_names};
use super::*;

/// The table with its three columns; the YouTube id is the key, so a second
/// row of the same song is refused (a block is per song, not per row).
#[tokio::test]
async fn v44_creates_the_block_list_keyed_by_youtube_id() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 43).await;
    apply_upto(&pool, 44).await;
    assert_eq!(
        column_names(&pool, "blocked_songs").await,
        ["youtube_id", "blocked_at_ms", "title"]
    );
    sqlx::query("INSERT INTO blocked_songs VALUES ('TwzfEwsTfag', 5, 'Never Lost')")
        .execute(&pool)
        .await
        .unwrap();
    let again = sqlx::query("INSERT INTO blocked_songs VALUES ('TwzfEwsTfag', 6, 'x')")
        .execute(&pool)
        .await;
    assert!(again.is_err(), "a second row of the same song is refused");
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 44, "{latest}");
}
