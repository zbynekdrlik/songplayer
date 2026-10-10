//! #223 S11: the swap — the rows' check and the file half.

use std::path::Path;

use super::*;

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn the_old_video_is_kept_under_its_name_plus_prev() {
    assert_eq!(
        prev_path(Path::new("/c/Song_Artist_PySFfTurafA_normalized_video.mp4")),
        Path::new("/c/Song_Artist_PySFfTurafA_normalized_video.mp4.prev")
    );
}

#[test]
fn the_temp_takes_the_videos_name_and_the_old_one_stays_as_prev() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("a_video.mp4");
    let temp = dir.path().join("PySFfTurafA_video_upgrade_temp.mp4");
    std::fs::write(&video, "old").unwrap();
    std::fs::write(&temp, "new").unwrap();
    assert_eq!(replace(&video, &temp), Swapped::Done);
    assert_eq!(read(&video), "new");
    assert_eq!(read(&prev_path(&video)), "old");
    assert!(!temp.exists());
}

#[test]
fn a_stale_prev_is_replaced_by_the_video_just_swapped_out() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("a_video.mp4");
    let temp = dir.path().join("t.mp4");
    std::fs::write(&video, "old").unwrap();
    std::fs::write(&temp, "new").unwrap();
    std::fs::write(prev_path(&video), "older").unwrap();
    assert_eq!(replace(&video, &temp), Swapped::Done);
    assert_eq!(read(&prev_path(&video)), "old");
}

/// A refused rename (here: no temp) changes nothing and leaves no `.prev`.
#[test]
fn a_refused_rename_is_busy_and_removes_the_link_again() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("a_video.mp4");
    std::fs::write(&video, "old").unwrap();
    let out = replace(&video, &dir.path().join("missing.mp4"));
    assert!(
        matches!(&out, Swapped::Busy(why) if why.starts_with("the rename over")),
        "{out:?}"
    );
    assert_eq!(read(&video), "old");
    assert!(!prev_path(&video).exists());
}

#[test]
fn a_missing_video_cannot_be_linked() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("a_video.mp4");
    let temp = dir.path().join("t.mp4");
    std::fs::write(&temp, "new").unwrap();
    let out = replace(&video, &temp);
    assert!(
        matches!(&out, Swapped::Refused(why) if why.starts_with("the link")),
        "{out:?}"
    );
    assert_eq!(read(&temp), "new");
    assert!(!video.exists());
}

async fn rows(paths: &[Option<&str>]) -> sqlx::SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    for (i, path) in paths.iter().enumerate() {
        let playlist = i64::try_from(i).unwrap() + 1;
        sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (?, 'p', ?)")
            .bind(playlist)
            .bind(format!("u{playlist}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, file_path) VALUES (?, 'PySFfTurafA', ?)",
        )
        .bind(playlist)
        .bind(*path)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool
}

/// Every row that names a video names the checked one (a row with none
/// does not count): the swap runs.
#[tokio::test]
async fn the_swap_runs_when_every_row_names_the_checked_video() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("a_video.mp4");
    let temp = dir.path().join("t.mp4");
    std::fs::write(&video, "old").unwrap();
    std::fs::write(&temp, "new").unwrap();
    let name = video.to_str().unwrap();
    let pool = rows(&[Some(name), Some(name), None]).await;
    assert_eq!(
        swap(&pool, "PySFfTurafA", &video, &temp).await,
        Swapped::Done
    );
    assert_eq!(read(&video), "new");
}

/// A row that names another video (renamed meanwhile), or no row naming
/// one: refused, nothing touched.
#[tokio::test]
async fn the_swap_is_refused_when_the_rows_moved_on() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("a_video.mp4");
    let temp = dir.path().join("t.mp4");
    std::fs::write(&video, "old").unwrap();
    std::fs::write(&temp, "new").unwrap();
    let name = video.to_str().unwrap();
    for paths in [vec![Some(name), Some("/c/b_video.mp4")], vec![None]] {
        let pool = rows(&paths).await;
        let out = swap(&pool, "PySFfTurafA", &video, &temp).await;
        assert!(
            matches!(&out, Swapped::Refused(why) if why.contains("no longer name")),
            "{paths:?}: {out:?}"
        );
        assert_eq!(read(&video), "old");
        assert_eq!(read(&temp), "new");
        assert!(!prev_path(&video).exists());
    }
}
