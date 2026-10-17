//! V43 (#261): the dead table goes, `play_history` is indexed by video,
//! and every playlist inactive at the migration starts #260's clock.

use super::test_helpers::{apply_first_n, apply_upto, column_names};
use super::*;

/// The table is gone, both indexes exist with their columns, the column is
/// new.
#[tokio::test]
async fn v43_drops_the_clip_mappings_and_indexes_the_plays_by_video() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 42).await;
    apply_upto(&pool, 43).await;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'resolume_clip_mappings'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(tables.is_empty(), "{tables:?}");
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_index_info('idx_play_history_video') ORDER BY seqno",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(columns, ["video_id", "playlist_id"]);
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_index_info('idx_videos_youtube') ORDER BY seqno",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(columns, ["youtube_id"]);
    assert!(
        column_names(&pool, "playlists")
            .await
            .contains(&"deactivated_at".to_string())
    );
}

/// The back-fill: an inactive playlist's unmarked rows carry its new
/// `deactivated_at`, a row the sync marked keeps its mark; an active
/// playlist and the test item's (even inactive) are left alone.
#[tokio::test]
async fn v43_starts_the_clock_of_every_inactive_playlist() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 42).await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, is_active, kind) VALUES \
         (1, 'on', 'u1', 1, 'youtube'), (2, 'off', 'u2', 0, 'youtube'), \
         (3, 'test', '', 0, 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, removed_at) VALUES \
         (11, 1, 'aaaaaaaaaaa', NULL), (12, 1, 'bbbbbbbbbbb', 1000), \
         (21, 2, 'aaaaaaaaaaa', NULL), (22, 2, 'ccccccccccc', 2000), \
         (31, 3, 'ddddddddddd', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let before = chrono::Utc::now().timestamp_millis() - 1_000;

    apply_upto(&pool, 43).await;

    let marks: Vec<(i64, Option<i64>)> =
        sqlx::query_as("SELECT id, deactivated_at FROM playlists ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    let off = marks[1].1.expect("the inactive playlist is stamped");
    assert!(off >= before, "{off} vs {before}");
    assert_eq!(marks, [(1, None), (2, Some(off)), (3, None)]);
    let rows: Vec<(i64, Option<i64>)> =
        sqlx::query_as("SELECT id, removed_at FROM videos ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [
            (11, None),
            (12, Some(1000)),
            (21, Some(off)),
            (22, Some(2000)),
            (31, None),
        ]
    );
}

#[tokio::test]
async fn migration_v43_is_in_the_list() {
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 43, "{latest}");
}
