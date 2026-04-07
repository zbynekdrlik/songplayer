//! Video selection with support for Continuous, Single, and Loop modes.

use rand::seq::SliceRandom;
use sp_core::playback::PlaybackMode;
use sqlx::SqlitePool;

use crate::db::models;

pub struct VideoSelector;

impl VideoSelector {
    /// Select next video for a playlist based on playback mode.
    /// Returns video ID (from videos table) or None if no videos available.
    pub async fn select_next(
        pool: &SqlitePool,
        playlist_id: i64,
        mode: PlaybackMode,
        current_video_id: Option<i64>,
    ) -> Result<Option<i64>, sqlx::Error> {
        match mode {
            PlaybackMode::Loop => {
                if let Some(id) = current_video_id {
                    return Ok(Some(id));
                }
                // First selection — pick random like Continuous.
                Self::select_random_unplayed(pool, playlist_id).await
            }
            PlaybackMode::Continuous | PlaybackMode::Single => {
                Self::select_random_unplayed(pool, playlist_id).await
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
}
