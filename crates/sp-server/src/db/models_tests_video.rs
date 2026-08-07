//! Video upsert/processing tests for `db::models` — `mark_video_processed_pair`
//! sidecar writes + empty-song rejection, `get_song_paths` read-back,
//! `suppress_resolume_en` flag round-trip, and Spotify resolution
//! bookkeeping. Split out of the former monolithic `models_tests.rs` (#137)
//! to keep every file under the 1000-line airuleset cap. Included as a
//! sibling file via `#[path = "models_tests_video.rs"] #[cfg(test)]
//! mod tests_video;` from `models.rs`.

#![allow(unused_imports)]

use super::tests_helpers::setup_with_video;
use super::*;

#[tokio::test]
async fn mark_video_processed_pair_writes_both_sidecar_paths() {
    let (pool, id) = setup_with_video().await;

    mark_video_processed_pair(
        &pool,
        id,
        "Amazing Grace",
        "Chris Tomlin",
        "gemini",
        false,
        "/cache/S_A_yt12345678_normalized_video.mp4",
        "/cache/S_A_yt12345678_normalized_audio.flac",
    )
    .await
    .unwrap();

    let row = sqlx::query(
        "SELECT song, artist, metadata_source, gemini_failed, file_path, audio_file_path, normalized
         FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(row.get::<String, _>("song"), "Amazing Grace");
    assert_eq!(row.get::<String, _>("artist"), "Chris Tomlin");
    assert_eq!(row.get::<String, _>("metadata_source"), "gemini");
    assert_eq!(row.get::<i64, _>("gemini_failed"), 0);
    assert_eq!(
        row.get::<String, _>("file_path"),
        "/cache/S_A_yt12345678_normalized_video.mp4"
    );
    assert_eq!(
        row.get::<String, _>("audio_file_path"),
        "/cache/S_A_yt12345678_normalized_audio.flac"
    );
    assert_eq!(row.get::<i64, _>("normalized"), 1);
}

#[tokio::test]
async fn mark_video_processed_pair_stores_gemini_failed_flag() {
    let (pool, id) = setup_with_video().await;
    mark_video_processed_pair(
        &pool,
        id,
        "S",
        "A",
        "parser",
        true,
        "/cache/v.mp4",
        "/cache/a.flac",
    )
    .await
    .unwrap();
    let gf: i64 = sqlx::query("SELECT gemini_failed FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("gemini_failed");
    assert_eq!(gf, 1);
}

/// `mark_video_processed_pair` is the write choke point for every
/// normalized video row. It must never write an empty `song` — a live
/// production defect shipped five rows with `song=""`, `artist=""`,
/// `gemini_failed=false` because nothing at this layer re-validated the
/// caller's (already-sanitized) metadata before the UPDATE. The row must
/// be left completely untouched (still unprocessed) on rejection.
#[tokio::test]
async fn mark_video_processed_pair_rejects_empty_song() {
    let (pool, id) = setup_with_video().await;

    let result = mark_video_processed_pair(
        &pool,
        id,
        "",
        "Some Artist",
        "gemini",
        false,
        "/cache/v.mp4",
        "/cache/a.flac",
    )
    .await;

    assert!(
        result.is_err(),
        "writing an empty song must be rejected, not silently written"
    );

    let row = sqlx::query("SELECT song, normalized FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let song: Option<String> = row.get("song");
    assert_eq!(
        song, None,
        "row must stay completely untouched, never written with an empty song"
    );
    assert_eq!(
        row.get::<i64, _>("normalized"),
        0,
        "row must stay unprocessed on rejection"
    );
}

/// A whitespace-only song is just as empty as `""` after trimming.
#[tokio::test]
async fn mark_video_processed_pair_rejects_whitespace_only_song() {
    let (pool, id) = setup_with_video().await;

    let result = mark_video_processed_pair(
        &pool,
        id,
        "   ",
        "Some Artist",
        "gemini",
        false,
        "/cache/v.mp4",
        "/cache/a.flac",
    )
    .await;

    assert!(
        result.is_err(),
        "a whitespace-only song must be rejected exactly like an empty one"
    );
}

#[tokio::test]
async fn get_song_paths_returns_both_when_normalized() {
    let (pool, id) = setup_with_video().await;
    mark_video_processed_pair(
        &pool,
        id,
        "S",
        "A",
        "parser",
        false,
        "/cache/video-path.mp4",
        "/cache/audio-path.flac",
    )
    .await
    .unwrap();

    let result = get_song_paths(&pool, id).await.unwrap();
    assert_eq!(
        result,
        Some((
            "/cache/video-path.mp4".to_string(),
            "/cache/audio-path.flac".to_string()
        ))
    );
}

#[tokio::test]
async fn get_song_paths_returns_none_when_unnormalized() {
    let (pool, id) = setup_with_video().await;
    // Row is unnormalized by default from setup_with_video.
    let result = get_song_paths(&pool, id).await.unwrap();
    assert_eq!(result, None);
}

#[tokio::test]
async fn get_song_paths_returns_none_when_audio_missing() {
    let (pool, id) = setup_with_video().await;
    // Mark normalized with only the video path; leave audio_file_path NULL.
    sqlx::query(
        "UPDATE videos SET normalized = 1, file_path = '/cache/v.mp4', audio_file_path = NULL
         WHERE id = ?",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    let result = get_song_paths(&pool, id).await.unwrap();
    assert_eq!(result, None);
}

#[tokio::test]
async fn get_song_paths_returns_none_for_nonexistent_id() {
    let (pool, _) = setup_with_video().await;
    let result = get_song_paths(&pool, 9999).await.unwrap();
    assert_eq!(result, None);
}

#[tokio::test]
async fn video_row_carries_suppress_resolume_en() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, suppress_resolume_en) \
         VALUES (1, 'yes_abc', 1), (1, 'no_xyz', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let videos = crate::db::models::get_videos_for_playlist(&pool, 1)
        .await
        .expect("fetch");
    let yes = videos
        .iter()
        .find(|v| v.youtube_id == "yes_abc")
        .expect("yes row");
    assert!(yes.suppress_resolume_en, "yes_abc must have flag=true");
    let no = videos
        .iter()
        .find(|v| v.youtube_id == "no_xyz")
        .expect("no row");
    assert!(!no.suppress_resolume_en, "no_xyz must have flag=false");
}

// Dedicated coverage for `get_video_suppress_resolume_en` — the playback
// engine's Resolume hot path calls this on every line change and needs
// both true and false outcomes to be distinguishable. Mutation testing
// caught that Ok(true) / Ok(false) / != vs == replacements passed through
// unnoticed because no test pinned the behaviour on a specific video_id.
#[tokio::test]
async fn get_video_suppress_resolume_en_returns_true_for_flagged_row() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, suppress_resolume_en) \
         VALUES (10, 1, 'on_video', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let flag = crate::db::models::get_video_suppress_resolume_en(&pool, 10)
        .await
        .expect("lookup ok");
    assert!(
        flag,
        "video with suppress_resolume_en=1 must return true, got {flag}"
    );
}

#[tokio::test]
async fn get_video_suppress_resolume_en_returns_false_for_unflagged_row() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, suppress_resolume_en) \
         VALUES (11, 1, 'off_video', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let flag = crate::db::models::get_video_suppress_resolume_en(&pool, 11)
        .await
        .expect("lookup ok");
    assert!(
        !flag,
        "video with suppress_resolume_en=0 must return false, got {flag}"
    );
}

#[tokio::test]
async fn get_video_suppress_resolume_en_returns_false_for_missing_row() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    let flag = crate::db::models::get_video_suppress_resolume_en(&pool, 9999)
        .await
        .expect("lookup must not error on missing id");
    assert!(
        !flag,
        "missing video row must default to false (no suppression), got {flag}"
    );
}

// ── set_video_spotify_resolution: rows_affected return value ──────────────
//
// Mutation testing on PR #74 surfaced two surviving mutants on this helper:
// `replace ... -> sqlx::Result<u64> with Ok(0)` and `with Ok(1)`. Both
// constants pass any test that doesn't assert on the return value. The
// tests below pin both ends: a successful UPDATE returns 1 (kills Ok(0))
// and an UPDATE with a non-existent video_id returns 0 (kills Ok(1)).

#[tokio::test]
async fn set_video_spotify_resolution_returns_one_on_successful_update() {
    let (pool, id) = setup_with_video().await;

    let n = set_video_spotify_resolution(&pool, id, Some("3n3Ppam7vgaVa1iaRUc9Lp"))
        .await
        .expect("update must not error");
    assert_eq!(n, 1, "UPDATE matching exactly one row must return 1");

    // Round-trip: the helper must actually have written both columns.
    let row = sqlx::query("SELECT spotify_track_id, spotify_resolved_at FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let stored: Option<String> = row.get("spotify_track_id");
    let resolved_at: Option<String> = row.get("spotify_resolved_at");
    assert_eq!(stored.as_deref(), Some("3n3Ppam7vgaVa1iaRUc9Lp"));
    assert!(
        resolved_at.is_some(),
        "spotify_resolved_at must be set to datetime('now')"
    );
}

#[tokio::test]
async fn set_video_spotify_resolution_returns_zero_when_id_missing() {
    let (pool, _id) = setup_with_video().await;

    let n = set_video_spotify_resolution(&pool, 9999, Some("3n3Ppam7vgaVa1iaRUc9Lp"))
        .await
        .expect("update of missing id must not error");
    assert_eq!(n, 0, "UPDATE matching zero rows must return 0");
}

#[tokio::test]
async fn set_video_spotify_resolution_persists_null_for_no_match_outcome() {
    let (pool, id) = setup_with_video().await;

    // Pre-set a value so we can verify the None call clears it.
    set_video_spotify_resolution(&pool, id, Some("3n3Ppam7vgaVa1iaRUc9Lp"))
        .await
        .unwrap();

    let n = set_video_spotify_resolution(&pool, id, None)
        .await
        .expect("update with None must not error");
    assert_eq!(n, 1, "UPDATE matching exactly one row must return 1");

    let row = sqlx::query("SELECT spotify_track_id, spotify_resolved_at FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let stored: Option<String> = row.get("spotify_track_id");
    let resolved_at: Option<String> = row.get("spotify_resolved_at");
    assert!(stored.is_none(), "None outcome must clear spotify_track_id");
    assert!(
        resolved_at.is_some(),
        "spotify_resolved_at must STILL be set on no-match (gates further re-resolution)"
    );
}
