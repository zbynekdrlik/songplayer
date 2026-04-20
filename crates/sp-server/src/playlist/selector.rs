//! Video selection with support for Continuous, Single, and Loop modes.

use rand::seq::SliceRandom;
use sp_core::playback::PlaybackMode;
use sqlx::SqlitePool;

use crate::db::models;

pub struct VideoSelector;

impl VideoSelector {
    /// Select next video for a playlist based on playback mode.
    /// Returns the video id (from `videos.id`) or `None` if nothing should
    /// play next. Custom playlists use `playlist_items` ordered by position
    /// and advance `playlists.current_position` as a side-effect.
    pub async fn select_next(
        pool: &SqlitePool,
        playlist_id: i64,
        mode: PlaybackMode,
        current_video_id: Option<i64>,
    ) -> Result<Option<i64>, sqlx::Error> {
        // Read the playlist kind to branch cleanly. Missing row → None.
        let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM playlists WHERE id = ?")
            .bind(playlist_id)
            .fetch_optional(pool)
            .await?;
        let Some(kind) = kind else { return Ok(None) };

        match kind.as_str() {
            "custom" => Self::select_next_custom(pool, playlist_id, mode, current_video_id).await,
            _ => match mode {
                PlaybackMode::Loop => {
                    if let Some(id) = current_video_id {
                        return Ok(Some(id));
                    }
                    Self::select_random_unplayed(pool, playlist_id).await
                }
                PlaybackMode::Continuous | PlaybackMode::Single => {
                    Self::select_random_unplayed(pool, playlist_id).await
                }
            },
        }
    }

    /// Custom playlist selection using `playlist_items` + `current_position`.
    async fn select_next_custom(
        pool: &SqlitePool,
        playlist_id: i64,
        mode: PlaybackMode,
        current_video_id: Option<i64>,
    ) -> Result<Option<i64>, sqlx::Error> {
        match mode {
            PlaybackMode::Loop => Ok(current_video_id),
            PlaybackMode::Single => Ok(None),
            PlaybackMode::Continuous => {
                let cur_pos: i64 =
                    sqlx::query_scalar("SELECT current_position FROM playlists WHERE id = ?")
                        .bind(playlist_id)
                        .fetch_one(pool)
                        .await?;

                // First call after a restart has current_video_id = None;
                // start from position 0 (cur_pos) instead of advancing past it.
                let next_pos = if current_video_id.is_none() {
                    cur_pos
                } else {
                    cur_pos + 1
                };

                let next_vid: Option<i64> = sqlx::query_scalar(
                    "SELECT video_id FROM playlist_items
                     WHERE playlist_id = ? AND position = ?",
                )
                .bind(playlist_id)
                .bind(next_pos)
                .fetch_optional(pool)
                .await?;

                if next_vid.is_some() {
                    sqlx::query("UPDATE playlists SET current_position = ? WHERE id = ?")
                        .bind(next_pos)
                        .bind(playlist_id)
                        .execute(pool)
                        .await?;
                }
                Ok(next_vid)
            }
        }
    }

    /// Pick a random normalized video that hasn't been played yet.
    /// If all have been played, clear history and start fresh.
    async fn select_random_unplayed(
        pool: &SqlitePool,
        playlist_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        let mut unplayed = models::get_unplayed_normalized_video_ids(pool, playlist_id).await?;

        if unplayed.is_empty() {
            // Check if there are any normalized videos at all.
            let all = models::get_normalized_video_ids(pool, playlist_id).await?;
            if all.is_empty() {
                return Ok(None);
            }
            // All played — clear history and start fresh.
            models::clear_play_history(pool, playlist_id).await?;
            unplayed = all;
        }

        let mut rng = rand::thread_rng();
        let chosen = unplayed.choose(&mut rng).copied();
        Ok(chosen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// Helper: create pool, run migrations, insert playlist + N normalized videos.
    async fn setup_with_videos(count: usize) -> (SqlitePool, i64, Vec<i64>) {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();

        let playlist = db::models::insert_playlist(&pool, "Test", "https://yt.com/pl")
            .await
            .unwrap();

        let mut video_ids = Vec::new();
        for i in 0..count {
            let vid = db::models::upsert_video(
                &pool,
                playlist.id,
                &format!("yt_{i}"),
                Some(&format!("Song {i}")),
            )
            .await
            .unwrap();

            // Mark as normalized.
            sqlx::query("UPDATE videos SET normalized = 1, file_path = ? WHERE id = ?")
                .bind(format!("/cache/song_{i}.mp4"))
                .bind(vid.id)
                .execute(&pool)
                .await
                .unwrap();

            video_ids.push(vid.id);
        }

        (pool, playlist.id, video_ids)
    }

    #[tokio::test]
    async fn continuous_selects_from_unplayed() {
        let (pool, playlist_id, video_ids) = setup_with_videos(3).await;

        let mut selected = std::collections::HashSet::new();
        for _ in 0..3 {
            let vid =
                VideoSelector::select_next(&pool, playlist_id, PlaybackMode::Continuous, None)
                    .await
                    .unwrap()
                    .expect("should select a video");

            assert!(video_ids.contains(&vid), "selected unknown video {vid}");
            selected.insert(vid);

            // Record play so next selection picks a different one.
            db::models::record_play(&pool, playlist_id, vid)
                .await
                .unwrap();
        }

        assert_eq!(selected.len(), 3, "all 3 videos should have been selected");
    }

    #[tokio::test]
    async fn continuous_resets_when_all_played() {
        let (pool, playlist_id, video_ids) = setup_with_videos(3).await;

        // Play all 3.
        for &vid_id in &video_ids {
            db::models::record_play(&pool, playlist_id, vid_id)
                .await
                .unwrap();
        }

        // Next selection should still work (history cleared internally).
        let vid = VideoSelector::select_next(&pool, playlist_id, PlaybackMode::Continuous, None)
            .await
            .unwrap();
        assert!(vid.is_some(), "should get a video after reset");
        assert!(video_ids.contains(&vid.unwrap()));
    }

    #[tokio::test]
    async fn continuous_single_video() {
        let (pool, playlist_id, video_ids) = setup_with_videos(1).await;

        let vid = VideoSelector::select_next(&pool, playlist_id, PlaybackMode::Continuous, None)
            .await
            .unwrap()
            .expect("should select the only video");
        assert_eq!(vid, video_ids[0]);

        // After playing it, reset should allow re-selection.
        db::models::record_play(&pool, playlist_id, vid)
            .await
            .unwrap();
        let vid2 = VideoSelector::select_next(&pool, playlist_id, PlaybackMode::Continuous, None)
            .await
            .unwrap()
            .expect("should re-select after reset");
        assert_eq!(vid2, video_ids[0]);
    }

    #[tokio::test]
    async fn loop_returns_current() {
        let (pool, playlist_id, video_ids) = setup_with_videos(3).await;

        let vid =
            VideoSelector::select_next(&pool, playlist_id, PlaybackMode::Loop, Some(video_ids[1]))
                .await
                .unwrap()
                .expect("should return current video");
        assert_eq!(vid, video_ids[1]);
    }

    #[tokio::test]
    async fn loop_first_selection() {
        let (pool, playlist_id, video_ids) = setup_with_videos(3).await;

        let vid = VideoSelector::select_next(&pool, playlist_id, PlaybackMode::Loop, None)
            .await
            .unwrap()
            .expect("should select a video for first loop play");
        assert!(video_ids.contains(&vid));
    }

    #[tokio::test]
    async fn empty_playlist_returns_none() {
        let (pool, playlist_id, _) = setup_with_videos(0).await;

        let vid = VideoSelector::select_next(&pool, playlist_id, PlaybackMode::Continuous, None)
            .await
            .unwrap();
        assert!(vid.is_none());
    }

    /// Helper: build a custom playlist with `count` items referencing the given
    /// (pre-normalized) video ids. Returns (pool, custom_playlist_id).
    async fn setup_custom_playlist_with_items(video_ids: &[i64]) -> (SqlitePool, i64) {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();

        // Seed a youtube playlist + videos so playlist_items FKs resolve.
        let yt = db::models::insert_playlist(&pool, "src", "https://yt.com/src")
            .await
            .unwrap();
        for (i, vid) in video_ids.iter().enumerate() {
            // Insert a video row with exactly the id we want (use a separate
            // INSERT with a chosen id since upsert_video auto-assigns).
            sqlx::query(
                "INSERT INTO videos (id, playlist_id, youtube_id, title, normalized, file_path, audio_file_path)
                 VALUES (?, ?, ?, ?, 1, ?, ?)",
            )
            .bind(*vid)
            .bind(yt.id)
            .bind(format!("yt_{i}"))
            .bind(format!("Song {i}"))
            .bind(format!("/cache/song_{i}_video.mp4"))
            .bind(format!("/cache/song_{i}_audio.flac"))
            .execute(&pool)
            .await
            .unwrap();
        }

        // Create a custom playlist (ytlive already exists, but make a fresh one
        // named "live-test" so the ytlive seed doesn't interfere).
        let custom_id: i64 = sqlx::query_scalar(
            "INSERT INTO playlists (name, youtube_url, ndi_output_name, playback_mode, is_active, kind)
             VALUES ('live-test', '', 'SP-live-test', 'continuous', 1, 'custom') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        for (pos, vid) in video_ids.iter().enumerate() {
            sqlx::query(
                "INSERT INTO playlist_items (playlist_id, video_id, position) VALUES (?, ?, ?)",
            )
            .bind(custom_id)
            .bind(*vid)
            .bind(pos as i64)
            .execute(&pool)
            .await
            .unwrap();
        }
        (pool, custom_id)
    }

    #[tokio::test]
    async fn custom_continuous_advances_through_items_then_stops() {
        let (pool, custom_id) = setup_custom_playlist_with_items(&[10, 20, 30]).await;

        // First call: no current video → return item at position 0 (10).
        let v1 = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Continuous, None)
            .await
            .unwrap();
        assert_eq!(v1, Some(10));

        // Second call: after playing 10 → advance to 20.
        let v2 = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Continuous, Some(10))
            .await
            .unwrap();
        assert_eq!(v2, Some(20));

        // Third call: 20 → 30.
        let v3 = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Continuous, Some(20))
            .await
            .unwrap();
        assert_eq!(v3, Some(30));

        // Past end — return None.
        let v4 = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Continuous, Some(30))
            .await
            .unwrap();
        assert_eq!(v4, None);
    }

    #[tokio::test]
    async fn custom_single_does_not_auto_advance() {
        let (pool, custom_id) = setup_custom_playlist_with_items(&[10, 20, 30]).await;
        let v = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Single, Some(10))
            .await
            .unwrap();
        assert_eq!(v, None);
    }

    #[tokio::test]
    async fn custom_loop_returns_current_video() {
        let (pool, custom_id) = setup_custom_playlist_with_items(&[10, 20, 30]).await;
        let v = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Loop, Some(20))
            .await
            .unwrap();
        assert_eq!(v, Some(20));
    }

    #[tokio::test]
    async fn custom_empty_playlist_returns_none() {
        let (pool, custom_id) = setup_custom_playlist_with_items(&[]).await;
        let v = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Continuous, None)
            .await
            .unwrap();
        assert_eq!(v, None);
    }
}
