//! #261: the stems queue and the video upgrade take no row of an inactive
//! playlist (`db::active_playlist!`), as the download and lyrics queues
//! already do. The inactive playlist's row is NOT marked here, so the
//! playlist's own state decides: it comes first (the lower id, the manual
//! priority, the on-program tier), and the queue takes the active one.

use sqlx::SqlitePool;

/// The rows of one test: the inactive playlist's, then the active one's.
struct Rows {
    pool: SqlitePool,
    inactive: i64,
    active: i64,
    inactive_playlist: i64,
}

/// Two downloaded videos (an audio sidecar, no stems): the first of an
/// inactive playlist, the second of an active one.
async fn rows() -> Rows {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, is_active) VALUES \
         (1, 'off', 'u1', 0), (2, 'on', 'u2', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut ids = Vec::new();
    for (playlist, youtube_id) in [(1, "inactive001"), (2, "active00001")] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, song, artist, normalized, \
             file_path, audio_file_path) VALUES (?, ?, 'Song', 'Artist', 1, ?, ?) RETURNING id",
        )
        .bind(playlist)
        .bind(youtube_id)
        .bind(format!("/cache/{youtube_id}_video.mp4"))
        .bind(format!("/cache/{youtube_id}_audio.flac"))
        .fetch_one(&pool)
        .await
        .unwrap();
        ids.push(id);
    }
    Rows {
        pool,
        inactive: ids[0],
        active: ids[1],
        inactive_playlist: 1,
    }
}

impl Rows {
    /// The active playlist is deactivated too (by hand, no marks).
    async fn deactivate_the_other(&self) {
        sqlx::query("UPDATE playlists SET is_active = 0 WHERE id = 2")
            .execute(&self.pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn the_stem_queue_never_takes_a_song_of_an_inactive_playlist() {
    use crate::db::{models_stems, models_stems_priority};
    let rows = rows().await;
    assert!(rows.inactive < rows.active);
    sqlx::query("UPDATE videos SET stem_manual_priority = 1 WHERE id = ?")
        .bind(rows.inactive)
        .execute(&rows.pool)
        .await
        .unwrap();
    let on_program = [rows.inactive_playlist];
    let next = models_stems_priority::get_next_stem_job(&rows.pool, &on_program, &[])
        .await
        .unwrap();
    assert_eq!(next.map(|j| j.video_id), Some(rows.active));
    let next = models_stems_priority::next_stem_for_playlists(&rows.pool, &on_program)
        .await
        .unwrap();
    assert!(next.is_none(), "tier 1 of the inactive playlist");
    let next = models_stems::get_next_video_for_stems(&rows.pool)
        .await
        .unwrap();
    assert_eq!(next.map(|j| j.video_id), Some(rows.active));
    let position = models_stems_priority::queue_position(&rows.pool, rows.inactive, &[], &[])
        .await
        .unwrap();
    assert_eq!(position, None, "no place in the queue");
    let position = models_stems_priority::queue_position(&rows.pool, rows.active, &[], &[])
        .await
        .unwrap();
    assert_eq!(position, Some(1), "first: nothing inactive before it");
    let counts = models_stems::count_stems_progress(&rows.pool)
        .await
        .unwrap();
    assert_eq!(
        counts,
        (1, 0),
        "the dashboard's stems counts: pending, done"
    );

    rows.deactivate_the_other().await;
    let next = models_stems_priority::get_next_stem_job(&rows.pool, &[], &[])
        .await
        .unwrap();
    assert!(next.is_none());
}

#[tokio::test]
async fn the_video_upgrade_never_takes_a_song_of_an_inactive_playlist() {
    use crate::video_upgrade::worker::{counts, next_song};
    let rows = rows().await;
    let next = next_song(&rows.pool, 2160, 1_760_000_000_000)
        .await
        .unwrap();
    assert_eq!(next.as_deref(), Some("active00001"));
    let pending = counts(&rows.pool, 2160).await.unwrap().pending;
    assert_eq!(pending, 1, "the status route's count");
    rows.deactivate_the_other().await;
    let next = next_song(&rows.pool, 2160, 1_760_000_000_000)
        .await
        .unwrap();
    assert!(next.is_none());
    assert_eq!(counts(&rows.pool, 2160).await.unwrap().pending, 0);
}
