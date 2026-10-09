//! V33 (#242): every playlist gets its own sound columns, at 0 dB and no EQ,
//! existing rows included.

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

#[tokio::test]
async fn v33_gives_every_playlist_0_db_and_no_eq() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 32).await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (33001, 'old', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    apply_upto(&pool, 33).await;
    let (gain, eq): (f64, String) =
        sqlx::query_as("SELECT audio_gain_db, audio_eq FROM playlists WHERE id = 33001")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((gain, eq.as_str()), (0.0, "[]"));
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (33002, 'new', 'u2')")
        .execute(&pool)
        .await
        .unwrap();
    let (gain, eq): (f64, String) =
        sqlx::query_as("SELECT audio_gain_db, audio_eq FROM playlists WHERE id = 33002")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((gain, eq.as_str()), (0.0, "[]"));
}
