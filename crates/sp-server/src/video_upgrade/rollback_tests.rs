//! #223 S12b: the old video put back after a failed open.

use std::path::Path;

use super::super::swap::prev_path;
use super::*;

const NOW: i64 = 1_760_000_000_000;

async fn new_pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'a', 'u1'), (2, 'b', 'u2')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// Two rows of one upgraded video (its V34 format recorded) and its files.
async fn upgraded(pool: &SqlitePool, video: &Path, state: &str) {
    for (id, playlist) in [(10, 1), (11, 2)] {
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, \
             video_format_id, video_height, video_upgrade_cap, video_upgrade_state, \
             video_upgrade_at) VALUES (?, ?, 'PySFfTurafA', 1, ?, '401', 2160, 2160, ?, 1)",
        )
        .bind(id)
        .bind(playlist)
        .bind(video.to_str().unwrap())
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
    }
}

type Row = (Option<i64>, Option<String>, Option<i64>, Option<String>);

async fn rows(pool: &SqlitePool) -> Vec<Row> {
    sqlx::query_as(
        "SELECT video_upgrade_cap, video_upgrade_state, video_upgrade_at, video_format_id \
         FROM videos ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_failed_open_of_an_upgraded_song_puts_the_old_video_back() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("a_video.mp4");
    std::fs::write(&video, "new").unwrap();
    std::fs::write(prev_path(&video), "old").unwrap();
    let pool = new_pool().await;
    upgraded(&pool, &video, "upgraded").await;
    assert_eq!(
        after_failed_open(&pool, 11, NOW).await,
        RolledBack::Restored
    );
    assert_eq!(std::fs::read_to_string(&video).unwrap(), "old");
    assert!(!prev_path(&video).exists());
    // Settled at the live cap (1440 here: no hardware decode), every row,
    // the format unknown again.
    let row = (Some(1440), Some("rolled_back".to_string()), Some(NOW), None);
    assert_eq!(rows(&pool).await, [row.clone(), row]);
}

/// Not upgraded, or upgraded with its `.prev` gone (it played since):
/// nothing changes.
#[tokio::test]
async fn any_other_failed_open_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("a_video.mp4");
    std::fs::write(&video, "new").unwrap();
    std::fs::write(prev_path(&video), "old").unwrap();
    let pool = new_pool().await;
    upgraded(&pool, &video, "no_better").await;
    assert_eq!(
        after_failed_open(&pool, 10, NOW).await,
        RolledBack::NotUpgraded
    );
    assert_eq!(std::fs::read_to_string(&video).unwrap(), "new");
    assert_eq!(
        after_failed_open(&pool, 99, NOW).await,
        RolledBack::NotUpgraded
    );

    let pool = new_pool().await;
    std::fs::remove_file(prev_path(&video)).unwrap();
    upgraded(&pool, &video, "upgraded").await;
    assert_eq!(
        after_failed_open(&pool, 10, NOW).await,
        RolledBack::NotUpgraded
    );
    assert_eq!(rows(&pool).await[0].1.as_deref(), Some("upgraded"));
}

#[test]
fn a_restore_that_cannot_rename_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("a_video.mp4");
    let why = restore(&prev_path(&video), &video).unwrap_err();
    assert!(why.starts_with("the rename of"), "{why}");
    assert!(!video.exists());
}
