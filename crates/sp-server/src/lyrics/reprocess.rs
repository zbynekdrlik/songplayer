//! 3-bucket priority queue for lyrics worker: manual > null-lyrics > stale-worst-first.

use anyhow::Result;
use sqlx::SqlitePool;

use crate::db::models::VideoLyricsRow;

/// Pick the next video the lyrics worker should process. Priority order:
/// 1. Manual-priority songs (user clicked "Reprocess")
/// 2. Null / failed lyrics (has_lyrics = 0): new songs + previously-failed
/// 3. Stale pipeline version, worst-quality first (NULLS FIRST)
///
/// Returns None when every active playlist song is current-version and
/// no manual queue entry is pending.
#[cfg_attr(test, mutants::skip)] // Priority ordering (manual > null > stale) exercised end-to-end by
// `manual_priority_beats_null_beats_stale`; per-bucket filters are
// individually mutation-tested via active/normalized/tiebreaker tests.
pub async fn get_next_video_for_lyrics(
    pool: &SqlitePool,
    current_version: u32,
) -> Result<Option<VideoLyricsRow>> {
    if let Some(row) = fetch_bucket_manual(pool).await? {
        return Ok(Some(row));
    }
    if let Some(row) = fetch_bucket_null(pool).await? {
        return Ok(Some(row));
    }
    fetch_bucket_stale(pool, current_version).await
}

async fn fetch_bucket_manual(pool: &SqlitePool) -> Result<Option<VideoLyricsRow>> {
    // Skip rows the worker has already tried and bailed on — mark_video_lyrics
    // on the failure path does NOT clear manual_priority, so without this
    // filter a failed manual-reprocess loops forever.
    let row = sqlx::query_as::<_, VideoLyricsRow>(
        "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
                COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, \
                p.youtube_url \
         FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.lyrics_manual_priority = 1 \
               AND (v.lyrics_source IS NULL \
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source')) \
               AND p.is_active = 1 AND v.normalized = 1 \
         ORDER BY v.id ASC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

async fn fetch_bucket_null(pool: &SqlitePool) -> Result<Option<VideoLyricsRow>> {
    // `lyrics_source NOT IN ('failed','empty','no_source')` skips rows that the
    // worker has already tried and bailed on — without this filter a song with
    // zero text sources (no yt_subs, no LRCLIB match, no description/CCLI yet)
    // gets picked every 10s forever, blocking every other null-lyric song
    // behind it. Matches the pre-refactor guard in get_next_video_without_lyrics.
    let row = sqlx::query_as::<_, VideoLyricsRow>(
        "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
                COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, \
                p.youtube_url \
         FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE (v.has_lyrics IS NULL OR v.has_lyrics = 0) \
               AND (v.lyrics_source IS NULL \
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source')) \
               AND v.lyrics_manual_priority = 0 \
               AND p.is_active = 1 AND v.normalized = 1 \
         ORDER BY v.id ASC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

async fn fetch_bucket_stale(
    pool: &SqlitePool,
    current_version: u32,
) -> Result<Option<VideoLyricsRow>> {
    let row = sqlx::query_as::<_, VideoLyricsRow>(
        "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
                COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, \
                p.youtube_url \
         FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.has_lyrics = 1 \
               AND v.lyrics_pipeline_version < ? \
               AND v.lyrics_manual_priority = 0 \
               AND p.is_active = 1 AND v.normalized = 1 \
         ORDER BY v.lyrics_quality_score ASC NULLS FIRST, v.id ASC LIMIT 1",
    )
    .bind(current_version as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Composite quality score written to `videos.lyrics_quality_score`. Higher = better.
/// Range typically in [-1.0, 1.0] but effectively [0.0, 1.0] for healthy alignments.
pub fn compute_quality_score(avg_confidence: f32, duplicate_start_pct: f32) -> f32 {
    avg_confidence - duplicate_start_pct / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{create_memory_pool, run_migrations};

    async fn setup() -> SqlitePool {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'active', 'u', 'n', 1), \
                    (2, 'inactive', 'u2', 'n2', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn manual_priority_beats_null_beats_stale() {
        let pool = setup().await;
        // Bucket 2: stale pipeline
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_pipeline_version, lyrics_quality_score, lyrics_manual_priority) \
             VALUES (1, 1, 'stale', 1, 1, 1, 0.1, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Bucket 1: null lyrics
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_pipeline_version, lyrics_manual_priority) \
             VALUES (2, 1, 'null1', 1, 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Bucket 0: manual priority
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_pipeline_version, lyrics_manual_priority) \
             VALUES (3, 1, 'manual', 1, 1, 2, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let row = get_next_video_for_lyrics(&pool, 2).await.unwrap().unwrap();
        assert_eq!(row.youtube_id, "manual", "manual bucket must win");

        // Clear manual — null wins next
        sqlx::query("UPDATE videos SET lyrics_manual_priority = 0 WHERE id = 3")
            .execute(&pool)
            .await
            .unwrap();
        let row = get_next_video_for_lyrics(&pool, 2).await.unwrap().unwrap();
        assert_eq!(
            row.youtube_id, "null1",
            "null bucket wins when manual is empty"
        );

        // Fill null — stale wins next
        sqlx::query("UPDATE videos SET has_lyrics = 1, lyrics_pipeline_version = 2 WHERE id = 2")
            .execute(&pool)
            .await
            .unwrap();
        let row = get_next_video_for_lyrics(&pool, 2).await.unwrap().unwrap();
        assert_eq!(
            row.youtube_id, "stale",
            "stale bucket wins when null is empty"
        );
    }

    #[tokio::test]
    async fn stale_bucket_orders_nulls_first_then_worst_quality() {
        let pool = setup().await;
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_pipeline_version, lyrics_quality_score) \
             VALUES (1, 1, 'good',   1, 1, 1, 0.9), \
                    (2, 1, 'bad',    1, 1, 1, 0.2), \
                    (3, 1, 'medium', 1, 1, 1, 0.5), \
                    (4, 1, 'null_q', 1, 1, 1, NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let row = get_next_video_for_lyrics(&pool, 2).await.unwrap().unwrap();
        assert_eq!(
            row.youtube_id, "null_q",
            "NULL quality score must come first"
        );

        sqlx::query("UPDATE videos SET lyrics_pipeline_version = 2 WHERE id = 4")
            .execute(&pool)
            .await
            .unwrap();
        let row = get_next_video_for_lyrics(&pool, 2).await.unwrap().unwrap();
        assert_eq!(row.youtube_id, "bad", "lowest quality score next");
    }

    #[tokio::test]
    async fn inactive_playlist_songs_are_never_returned() {
        let pool = setup().await;
        // One song per bucket, all on inactive playlist (id=2)
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_pipeline_version, lyrics_quality_score, lyrics_manual_priority) \
             VALUES \
                 (2, 'inactive_manual', 1, 0, 0, NULL, 1), \
                 (2, 'inactive_null',   1, 0, 0, NULL, 0), \
                 (2, 'inactive_stale',  1, 1, 1, 0.1, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            get_next_video_for_lyrics(&pool, 2).await.unwrap().is_none(),
            "no song from an inactive playlist should ever be returned, regardless of bucket"
        );
    }

    #[tokio::test]
    async fn unnormalized_videos_are_never_returned() {
        let pool = setup().await;
        // One song per bucket, all un-normalized
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_pipeline_version, lyrics_quality_score, lyrics_manual_priority) \
             VALUES \
                 (1, 'unnorm_manual', 0, 0, 0, NULL, 1), \
                 (1, 'unnorm_null',   0, 0, 0, NULL, 0), \
                 (1, 'unnorm_stale',  0, 1, 1, 0.1, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            get_next_video_for_lyrics(&pool, 2).await.unwrap().is_none(),
            "un-normalized videos must be filtered from every bucket"
        );
    }

    #[tokio::test]
    async fn manual_bucket_skips_failed_songs_so_user_reprocess_does_not_loop() {
        // Regression: without this, clicking "Reprocess" on a song that has
        // no text sources (no yt_subs, no LRCLIB) would loop forever in
        // bucket 0 — mark_video_lyrics on failure does not clear
        // manual_priority, so the selector re-picks it on every tick.
        // The selector must skip rows marked as previously-failed so the
        // manual queue advances even for no-source songs.
        let pool = setup().await;
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_source, lyrics_manual_priority) VALUES \
                 (1, 1, 'manual_failed', 1, 0, 'no_source', 1), \
                 (2, 1, 'manual_retry',  1, 0, NULL,        1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let row = get_next_video_for_lyrics(&pool, 2).await.unwrap().unwrap();
        assert_eq!(
            row.youtube_id, "manual_retry",
            "manual bucket must skip previously-failed songs so user-triggered reprocess doesn't loop"
        );
    }

    #[tokio::test]
    async fn null_bucket_skips_failed_songs_so_queue_advances() {
        let pool = setup().await;
        // Both rows look like failed attempts (has_lyrics=0) but only one has
        // been tried; the other has been bailed on with a failure marker. The
        // selector must skip the failed one so the queue moves forward.
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_source) VALUES \
                 (1, 1, 'previously_failed', 1, 0, 'no_source'), \
                 (2, 1, 'never_tried',       1, 0, NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let row = get_next_video_for_lyrics(&pool, 2).await.unwrap().unwrap();
        assert_eq!(
            row.youtube_id, "never_tried",
            "previously-failed songs must not block the queue"
        );
    }

    #[tokio::test]
    async fn stale_bucket_tiebreaks_by_id_when_quality_equal() {
        let pool = setup().await;
        // Both rows have identical quality score; lower id must win.
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_pipeline_version, lyrics_quality_score) \
             VALUES \
                 (10, 1, 'same_q_hi_id', 1, 1, 1, 0.5), \
                 (5,  1, 'same_q_lo_id', 1, 1, 1, 0.5)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let row = get_next_video_for_lyrics(&pool, 2).await.unwrap().unwrap();
        assert_eq!(
            row.youtube_id, "same_q_lo_id",
            "when quality scores tie, lower v.id must win"
        );
    }

    #[tokio::test]
    async fn returns_none_when_all_current() {
        let pool = setup().await;
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_pipeline_version) VALUES (1, 'ok', 1, 1, 2)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(get_next_video_for_lyrics(&pool, 2).await.unwrap().is_none());
    }

    #[test]
    fn quality_score_formula() {
        assert!((compute_quality_score(0.8, 10.0) - 0.7).abs() < 1e-6);
        assert!((compute_quality_score(0.5, 50.0) - 0.0).abs() < 1e-6);
        assert!((compute_quality_score(0.9, 0.0) - 0.9).abs() < 1e-6);
    }
}
