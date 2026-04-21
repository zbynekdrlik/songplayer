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
    if let Some(row) = fetch_bucket_manual(pool, current_version).await? {
        return Ok(Some(row));
    }
    if let Some(row) = fetch_bucket_null(pool, current_version).await? {
        return Ok(Some(row));
    }
    fetch_bucket_stale(pool, current_version).await
}

async fn fetch_bucket_manual(
    pool: &SqlitePool,
    current_version: u32,
) -> Result<Option<VideoLyricsRow>> {
    // Skip rows the worker has already tried and bailed on — mark_video_lyrics
    // on the failure path does NOT clear manual_priority, so without this
    // filter a failed manual-reprocess loops forever.
    // Exception: if a row's recorded failure is from an OLDER pipeline version,
    // allow it through — the worker may have new capability that succeeds now.
    let row = sqlx::query_as::<_, VideoLyricsRow>(
        "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
                COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, \
                p.youtube_url \
         FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.lyrics_manual_priority = 1 \
               AND (v.lyrics_source IS NULL \
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source') \
                    OR v.lyrics_pipeline_version < ?) \
               AND p.is_active = 1 AND v.normalized = 1 \
         ORDER BY v.id ASC LIMIT 1",
    )
    .bind(current_version as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

async fn fetch_bucket_null(
    pool: &SqlitePool,
    current_version: u32,
) -> Result<Option<VideoLyricsRow>> {
    // `lyrics_source NOT IN ('failed','empty','no_source')` skips rows that the
    // worker has already tried and bailed on — without this filter a song with
    // zero text sources (no yt_subs, no LRCLIB match, no description/CCLI yet)
    // gets picked every 10s forever, blocking every other null-lyric song
    // behind it. Matches the pre-refactor guard in get_next_video_without_lyrics.
    // Exception: if a row's recorded failure is from an OLDER pipeline version,
    // allow it through — the worker may have new capability (e.g., a new
    // provider added in the version bump) that succeeds where prior runs failed.
    let row = sqlx::query_as::<_, VideoLyricsRow>(
        "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
                COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, \
                p.youtube_url \
         FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE (v.has_lyrics IS NULL OR v.has_lyrics = 0) \
               AND (v.lyrics_source IS NULL \
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source') \
                    OR v.lyrics_pipeline_version < ?) \
               AND v.lyrics_manual_priority = 0 \
               AND p.is_active = 1 AND v.normalized = 1 \
         ORDER BY v.id ASC LIMIT 1",
    )
    .bind(current_version as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

async fn fetch_bucket_stale(
    pool: &SqlitePool,
    current_version: u32,
) -> Result<Option<VideoLyricsRow>> {
    // v18 smart-skip clause: `NOT (source LIKE '%gemini%' AND version >= 18)`.
    // Pre-v18 Gemini output is degraded in one or more ways:
    //   - v11-v14: sanitize_track dropped wordless lines → empty JSONs.
    //   - v15: sanitize fixed, but AutoSubProvider still registered →
    //     autosub contamination.
    //   - v16: AutoSub removed, but no end_ms clip / no merge break.
    //   - v17: end_ms clip + merge break added, but also synthesized
    //     per-word timings by even-distribution. The fake timings
    //     animated wrong on the karaoke wall; user asked for
    //     line-level focus only.
    //   - v18+: `words: None` for wordless providers; end_ms clip
    //     and merge break retained. Line timing is clean; no fake
    //     per-word data.
    let row = sqlx::query_as::<_, VideoLyricsRow>(
        "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
                COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, \
                p.youtube_url \
         FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.has_lyrics = 1 \
               AND v.lyrics_pipeline_version < ? \
               AND v.lyrics_manual_priority = 0 \
               AND NOT (v.lyrics_source LIKE '%gemini%' AND v.lyrics_pipeline_version >= 18) \
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
             lyrics_source, lyrics_pipeline_version, lyrics_manual_priority) VALUES \
                 (1, 1, 'manual_failed', 1, 0, 'no_source', 2, 1), \
                 (2, 1, 'manual_retry',  1, 0, NULL,        0, 1)",
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
             lyrics_source, lyrics_pipeline_version) VALUES \
                 (1, 1, 'previously_failed', 1, 0, 'no_source', 2), \
                 (2, 1, 'never_tried',       1, 0, NULL,        0)",
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
    async fn null_bucket_retries_failed_songs_when_pipeline_version_bumps() {
        // Regression: v4→v5 added description provider. Songs that failed under
        // v4 (marked 'no_source' because yt_subs/lrclib/autosub all missed) must
        // be retried on v5 because the worker now has new capability that might
        // succeed. The previous filter locked them out forever.
        let pool = setup().await;
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_source, lyrics_pipeline_version) VALUES \
                 (1, 1, 'failed_v4', 1, 0, 'no_source', 4), \
                 (2, 1, 'fresh',     1, 0, NULL,        0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // With current pipeline version = 5, both rows must be eligible.
        // Verify the previously-failed row is not locked out by checking that
        // after picking and "completing" the fresh row, the old-version failure
        // is then returned.
        let row = fetch_bucket_null(&pool, 5).await.unwrap().unwrap();
        // Mark the first picked row as done so we can check what comes next.
        sqlx::query(
            "UPDATE videos SET has_lyrics = 1, lyrics_pipeline_version = 5 WHERE youtube_id = ?",
        )
        .bind(&row.youtube_id)
        .execute(&pool)
        .await
        .unwrap();
        let row2 = fetch_bucket_null(&pool, 5).await.unwrap();
        assert!(
            row2.is_some(),
            "previously-failed v4 row must be retried under v5 pipeline"
        );
    }

    #[tokio::test]
    async fn null_bucket_still_skips_current_version_failures_to_avoid_loops() {
        // The OTHER regression guard: a song marked 'no_source' UNDER THE CURRENT
        // pipeline version must still be skipped, otherwise the worker loops
        // forever on the same failing song. Only older-version failures get retry.
        let pool = setup().await;
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_source, lyrics_pipeline_version) VALUES \
                 (1, 1, 'failed_v5', 1, 0, 'no_source', 5), \
                 (2, 1, 'fresh',     1, 0, NULL,        0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let row = fetch_bucket_null(&pool, 5).await.unwrap().unwrap();
        assert_eq!(
            row.youtube_id, "fresh",
            "must pick the fresh row, NOT the current-version failure"
        );
    }

    #[tokio::test]
    async fn manual_bucket_retries_failed_songs_when_pipeline_version_bumps() {
        // Same fix on the manual bucket: user-triggered reprocess of a previously
        // failed song under an OLDER pipeline version must retry, not short-circuit.
        let pool = setup().await;
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_source, lyrics_pipeline_version, lyrics_manual_priority) VALUES \
                 (1, 1, 'failed_v4_manual', 1, 0, 'no_source', 4, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let row = fetch_bucket_manual(&pool, 5).await.unwrap();
        assert!(
            row.is_some(),
            "manual bucket must retry older-pipeline failures on version bump"
        );
    }

    #[tokio::test]
    async fn stale_bucket_skips_songs_already_produced_by_gemini() {
        // v18 smart-skip clause: `NOT (source LIKE '%gemini%' AND version >= 18)`.
        // Pre-v18 Gemini output is degraded (empty lines, autosub contamination,
        // missing end_ms clip, or synthesized fake words). Only v18+ rows trusted.
        let pool = setup().await;
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_source, lyrics_pipeline_version, lyrics_quality_score) VALUES \
                 (1, 1, 'gemini_v18',   1, 1, 'ensemble:gemini',         18, 0.9), \
                 (2, 1, 'gemini_v17',   1, 1, 'ensemble:gemini',         17, 0.8), \
                 (3, 1, 'autosub_only', 1, 1, 'ensemble:autosub',        15, 0.4), \
                 (4, 1, 'old_gemini',   1, 1, 'ensemble:gemini',         14, 0.2)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Running under v19 — only the v18 pure-Gemini row is protected; all
        // pre-v18 rows must come back, including v17 (which had fake words).
        let mut remaining = vec!["gemini_v17", "autosub_only", "old_gemini"];
        while !remaining.is_empty() {
            let row = fetch_bucket_stale(&pool, 19).await.unwrap().unwrap();
            assert!(
                remaining.contains(&row.youtube_id.as_str()),
                "unexpected row picked: {}",
                row.youtube_id
            );
            remaining.retain(|&id| id != row.youtube_id.as_str());
            sqlx::query("UPDATE videos SET lyrics_pipeline_version = 19 WHERE youtube_id = ?")
                .bind(&row.youtube_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert!(
            fetch_bucket_stale(&pool, 19).await.unwrap().is_none(),
            "v18+ pure-Gemini rows must not appear in stale bucket"
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
