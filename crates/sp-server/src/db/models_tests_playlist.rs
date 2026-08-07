//! Playlist CRUD tests for `db::models` — playlist creation, active-playlist
//! reads, playlist-item append/remove/reorder, position lookup, and
//! membership checks. Split out of the former monolithic `models_tests.rs`
//! (#137) to keep every file under the 1000-line airuleset cap. Included as
//! a sibling file via `#[path = "models_tests_playlist.rs"] #[cfg(test)]
//! mod tests_playlist;` from `models.rs`.

#![allow(unused_imports)]

use super::*;
use crate::db;

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

/// #134: video_playlist_membership is the youtube-kind counterpart of
/// position_for_playlist_item — direct videos.playlist_id membership + the
/// normalized flag, instead of the playlist_items set-list table.
#[tokio::test]
async fn video_playlist_membership_reports_normalized_and_ownership() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let other = insert_playlist(&pool, "other", "https://yt.com/other")
        .await
        .unwrap();
    let a = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
    sqlx::query("UPDATE videos SET normalized = 1 WHERE id = ?")
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    let not_ready = upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;

    // Belongs + normalized.
    let m = video_playlist_membership(&pool, yt.id, a).await.unwrap();
    assert_eq!(m, Some(true));

    // Belongs but not normalized.
    let m = video_playlist_membership(&pool, yt.id, not_ready)
        .await
        .unwrap();
    assert_eq!(m, Some(false));

    // Video exists but belongs to a DIFFERENT playlist.
    let m = video_playlist_membership(&pool, other.id, a).await.unwrap();
    assert_eq!(
        m, None,
        "must not report membership for a video belonging to a different playlist"
    );

    // Video doesn't exist at all.
    let m = video_playlist_membership(&pool, yt.id, 999_999)
        .await
        .unwrap();
    assert_eq!(m, None);
}
