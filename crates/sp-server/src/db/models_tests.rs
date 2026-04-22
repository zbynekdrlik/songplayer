//! Tests for `db::models` (playlist/video CRUD helpers). Included as a
//! sibling file via `#[path = "models_tests.rs"] #[cfg(test)] mod tests;`
//! from `models.rs` to keep that file under the 1000-line airuleset cap.

#![allow(unused_imports)]

use super::*;
use crate::db;

async fn setup_with_video() -> (SqlitePool, i64) {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Insert an unnormalized video row; the tests will mark it processed.
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized) VALUES (1, 'yt123', 't', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = sqlx::query("SELECT id FROM videos WHERE youtube_id = 'yt123'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let id: i64 = row.get("id");
    (pool, id)
}

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
async fn mark_video_lyrics_complete_writes_all_fields() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, lyrics_manual_priority) \
                 VALUES (1, 99, 'abc', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    mark_video_lyrics_complete(&pool, 1, "ensemble:qwen3+autosub", 2, Some(0.85))
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version, \
         lyrics_quality_score, lyrics_manual_priority FROM videos WHERE id = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(row.get::<i64, _>("has_lyrics"), 1);
    assert_eq!(
        row.get::<String, _>("lyrics_source"),
        "ensemble:qwen3+autosub"
    );
    assert_eq!(row.get::<i64, _>("lyrics_pipeline_version"), 2);
    assert!((row.get::<f64, _>("lyrics_quality_score") - 0.85).abs() < 1e-3);
    assert_eq!(
        row.get::<i64, _>("lyrics_manual_priority"),
        0,
        "manual_priority must be cleared on successful processing"
    );
}

#[tokio::test]
async fn mark_complete_with_none_quality_writes_null_not_zero() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized) \
                 VALUES (1, 99, 'abc', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    mark_video_lyrics_complete(&pool, 1, "yt_subs", 2, None)
        .await
        .unwrap();

    let q: Option<f64> = sqlx::query_scalar("SELECT lyrics_quality_score FROM videos WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        q, None,
        "fallback path must write NULL, not 0.0 — 0.0 poisons the NULLS FIRST queue ordering"
    );
}

#[tokio::test]
async fn mark_video_lyrics_stamps_pipeline_version_on_failure() {
    // Regression guard for the infinite-loop production bug: a song that fails
    // processing must record the current pipeline version, otherwise the null
    // bucket's `OR lyrics_pipeline_version < current` retry clause brings it
    // back every poll because the default version (0) is always < current.
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized) \
                 VALUES (1, 99, 'loop_me', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    mark_video_lyrics(&pool, 1, false, Some("no_source"), 5)
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version FROM videos WHERE id = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("has_lyrics"), 0);
    assert_eq!(row.get::<String, _>("lyrics_source"), "no_source");
    assert_eq!(
        row.get::<i64, _>("lyrics_pipeline_version"),
        5,
        "failure write MUST stamp current pipeline version or the retry \
         filter will loop the song forever (0 < current is always true)"
    );
}

#[tokio::test]
async fn get_active_playlists_includes_ytlive_with_kind_custom() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    crate::startup::ensure_live_playlist_exists(&pool)
        .await
        .unwrap();
    let active = get_active_playlists(&pool).await.unwrap();
    let ytlive = active
        .iter()
        .find(|p| p.name == "ytlive")
        .expect("ytlive should exist after ensure_live_playlist_exists");
    assert_eq!(ytlive.kind, "custom");
    assert_eq!(ytlive.current_position, 0);
    assert_eq!(ytlive.ndi_output_name, "SP-live");
}

#[tokio::test]
async fn insert_playlist_defaults_kind_to_youtube() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    let created = insert_playlist(&pool, "TestYT", "https://yt.com/test")
        .await
        .unwrap();
    assert_eq!(created.kind, "youtube");
    assert_eq!(created.current_position, 0);
}

#[tokio::test]
async fn append_item_assigns_next_position() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    crate::startup::ensure_live_playlist_exists(&pool)
        .await
        .unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let v1 = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
    let v2 = upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;

    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let p1 = append_playlist_item(&pool, ytlive_id, v1).await.unwrap();
    let p2 = append_playlist_item(&pool, ytlive_id, v2).await.unwrap();
    assert_eq!(p1, 0);
    assert_eq!(p2, 1);
}

#[tokio::test]
async fn append_item_duplicate_errors() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    crate::startup::ensure_live_playlist_exists(&pool)
        .await
        .unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let v = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();

    append_playlist_item(&pool, ytlive_id, v).await.unwrap();
    let err = append_playlist_item(&pool, ytlive_id, v).await;
    assert!(err.is_err(), "duplicate append must error");
}

#[tokio::test]
async fn remove_item_compacts_positions() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    crate::startup::ensure_live_playlist_exists(&pool)
        .await
        .unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let v1 = upsert_video(&pool, yt.id, "id1", Some("A"))
        .await
        .unwrap()
        .id;
    let v2 = upsert_video(&pool, yt.id, "id2", Some("B"))
        .await
        .unwrap()
        .id;
    let v3 = upsert_video(&pool, yt.id, "id3", Some("C"))
        .await
        .unwrap()
        .id;
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();

    append_playlist_item(&pool, ytlive_id, v1).await.unwrap();
    append_playlist_item(&pool, ytlive_id, v2).await.unwrap();
    append_playlist_item(&pool, ytlive_id, v3).await.unwrap();

    remove_playlist_item(&pool, ytlive_id, v2).await.unwrap();

    let items = list_playlist_items(&pool, ytlive_id).await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].position, 0);
    assert_eq!(items[0].video_id, v1);
    assert_eq!(items[1].position, 1);
    assert_eq!(items[1].video_id, v3);
}

#[tokio::test]
async fn list_playlist_items_returns_rows_in_position_order() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    crate::startup::ensure_live_playlist_exists(&pool)
        .await
        .unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let a = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
    let b = upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();

    append_playlist_item(&pool, ytlive_id, a).await.unwrap();
    append_playlist_item(&pool, ytlive_id, b).await.unwrap();

    let items = list_playlist_items(&pool, ytlive_id).await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].video_id, a);
    assert_eq!(items[1].video_id, b);
}

/// Mutation-coverage: if the `playback_mode` assignment in
/// `get_active_playlists` is deleted, this test catches it because the
/// ytlive seed row has `playback_mode='continuous'` but the Default impl
/// would produce an empty string. Also pins `current_position` read.
#[tokio::test]
async fn get_active_playlists_reads_playback_mode_from_row() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    crate::startup::ensure_live_playlist_exists(&pool)
        .await
        .unwrap();
    // Set a non-default current_position so we can distinguish DB value from Default (0).
    sqlx::query("UPDATE playlists SET current_position = 7 WHERE name = 'ytlive'")
        .execute(&pool)
        .await
        .unwrap();

    let active = get_active_playlists(&pool).await.unwrap();
    let ytlive = active
        .iter()
        .find(|p| p.name == "ytlive")
        .expect("ytlive must be active");
    assert_eq!(
        ytlive.playback_mode, "continuous",
        "get_active_playlists must read playback_mode from the row, not use Default"
    );
    assert_eq!(
        ytlive.current_position, 7,
        "get_active_playlists must read current_position from the row, not use Default"
    );
}

/// Mutation-coverage: insert_playlist's struct init for playback_mode, kind,
/// and current_position must come from the RETURNING row, not fall back to
/// `Default`. To distinguish: after insert we UPDATE the row to non-default
/// values, then assert get_active_playlists returns the updated values.
#[tokio::test]
async fn insert_playlist_materialises_playback_mode_and_kind_and_current_position() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();

    // Insert via our helper — schema defaults: playback_mode='continuous',
    // kind='youtube', current_position=0.
    let created = insert_playlist(&pool, "TestYT", "https://yt.com/test")
        .await
        .unwrap();
    assert_eq!(
        created.playback_mode, "continuous",
        "insert_playlist must read playback_mode from RETURNING row"
    );
    assert_eq!(
        created.kind, "youtube",
        "insert_playlist must read kind from RETURNING row"
    );
    assert_eq!(
        created.current_position, 0,
        "insert_playlist must read current_position from RETURNING row"
    );

    // Now mutate to non-default values and confirm get_active_playlists reads them.
    sqlx::query(
        "UPDATE playlists SET current_position = 42, playback_mode = 'single', is_active = 1
         WHERE name = 'TestYT'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let active = get_active_playlists(&pool).await.unwrap();
    let test = active
        .iter()
        .find(|p| p.name == "TestYT")
        .expect("TestYT must be active");
    assert_eq!(
        test.current_position, 42,
        "must read updated current_position from DB, not Default"
    );
    assert_eq!(
        test.playback_mode, "single",
        "must read updated playback_mode from DB, not Default"
    );
}

#[tokio::test]
async fn position_for_video_lookup() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    crate::startup::ensure_live_playlist_exists(&pool)
        .await
        .unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let a = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
    let b = upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();
    append_playlist_item(&pool, ytlive_id, a).await.unwrap();
    append_playlist_item(&pool, ytlive_id, b).await.unwrap();

    let pos = position_for_playlist_item(&pool, ytlive_id, b)
        .await
        .unwrap();
    assert_eq!(pos, Some(1));

    let missing = position_for_playlist_item(&pool, ytlive_id, 999)
        .await
        .unwrap();
    assert_eq!(missing, None);
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
