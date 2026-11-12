//! #265: the block list's queries, and the selection that leaves a blocked
//! song out (the one pick predicate `db::pickable_row!`), while an
//! operator's set list still plays it.

use sp_core::playback::PlaybackMode;
use sqlx::SqlitePool;

use super::{block, list, song_title, unblock};
use crate::db::models;
use crate::playlist::loop_memory;
use crate::playlist::selector::VideoSelector;

async fn memory_pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

/// A youtube playlist `name` with downloaded songs `yt_<i>` for each `i`;
/// returns the playlist id and the row ids in order.
async fn songs(pool: &SqlitePool, name: &str, songs: &[usize]) -> (i64, Vec<i64>) {
    let playlist = models::insert_playlist(pool, name, &format!("https://yt.com/{name}"))
        .await
        .unwrap();
    let mut ids = Vec::new();
    for i in songs {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, title, normalized, file_path, \
             audio_file_path) VALUES (?, ?, ?, 1, ?, ?) RETURNING id",
        )
        .bind(playlist.id)
        .bind(format!("yt_{i}"))
        .bind(format!("YouTube title {i}"))
        .bind(format!("/cache/song_{i}_video.mp4"))
        .bind(format!("/cache/song_{i}_audio.flac"))
        .fetch_one(pool)
        .await
        .unwrap();
        ids.push(id);
    }
    (playlist.id, ids)
}

async fn block_yt(pool: &SqlitePool, i: usize) {
    block(pool, &format!("yt_{i}"), "t", 1).await.unwrap();
}

#[tokio::test]
async fn a_block_keeps_its_first_time_and_title_and_says_whether_it_added() {
    let pool = memory_pool().await;
    let (first, added) = block(&pool, "yt_1", "Never Lost - Elevation", 100)
        .await
        .unwrap();
    assert!(added);
    assert_eq!(
        (
            first.youtube_id.as_str(),
            first.title.as_str(),
            first.blocked_at_ms
        ),
        ("yt_1", "Never Lost - Elevation", 100)
    );
    let (again, added) = block(&pool, "yt_1", "other", 200).await.unwrap();
    assert!(!added);
    assert_eq!(again, first);
    block(&pool, "yt_2", "Bravo", 300).await.unwrap();
    let listed: Vec<String> = list(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.youtube_id)
        .collect();
    assert_eq!(listed, ["yt_2", "yt_1"], "the newest block first");
}

#[tokio::test]
async fn an_unblock_answers_the_title_once() {
    let pool = memory_pool().await;
    block(&pool, "yt_1", "Never Lost", 1).await.unwrap();
    assert_eq!(
        unblock(&pool, "yt_1").await.unwrap().as_deref(),
        Some("Never Lost")
    );
    assert_eq!(unblock(&pool, "yt_1").await.unwrap(), None);
    assert!(list(&pool).await.unwrap().is_empty());
}

/// The wall's text from a row with a song name, else the YouTube title;
/// none for a song this node does not have or only as the test item.
#[tokio::test]
async fn the_song_title_is_the_wall_s_text_from_this_node_s_rows() {
    let pool = memory_pool().await;
    let (_, ids) = songs(&pool, "fast", &[1, 2]).await;
    assert_eq!(
        song_title(&pool, "yt_1").await.unwrap().as_deref(),
        Some("YouTube title 1")
    );
    // A second row of the same song (another playlist) has the metadata.
    let (_, other) = songs(&pool, "slow", &[1]).await;
    sqlx::query("UPDATE videos SET song = 'Never Lost', artist = 'Elevation' WHERE id = ?")
        .bind(other[0])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        song_title(&pool, "yt_1").await.unwrap().as_deref(),
        Some("Never Lost - Elevation")
    );
    sqlx::query("UPDATE videos SET song = 'Bravo' WHERE id = ?")
        .bind(ids[1])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        song_title(&pool, "yt_2").await.unwrap().as_deref(),
        Some("Bravo")
    );
    assert_eq!(song_title(&pool, "nope").await.unwrap(), None);
    let test: i64 = sqlx::query_scalar(
        "INSERT INTO playlists (name, youtube_url, kind) VALUES ('SP-test', '', 'test') \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO videos (playlist_id, youtube_id, title) VALUES (?, 'clip', 'c')")
        .bind(test)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(song_title(&pool, "clip").await.unwrap(), None);
}

/// Thirty picks out of three songs, one blocked (by a row of ANOTHER
/// playlist's id: the block is per song) — a random pick cannot pass by luck.
#[tokio::test]
async fn continuous_never_picks_a_blocked_song() {
    let pool = memory_pool().await;
    let (playlist, ids) = songs(&pool, "fast", &[0, 1, 2]).await;
    songs(&pool, "slow", &[0]).await;
    block_yt(&pool, 0).await;
    block_yt(&pool, 2).await;
    for _ in 0..30 {
        let pick = VideoSelector::select_next(&pool, playlist, PlaybackMode::Continuous, None, &[])
            .await
            .unwrap();
        assert_eq!(pick, Some(ids[1]));
    }
    assert_eq!(
        models::get_normalized_video_ids(&pool, playlist)
            .await
            .unwrap(),
        vec![ids[1]]
    );
    // Unblocked, it is picked again.
    unblock(&pool, "yt_2").await.unwrap();
    assert_eq!(
        models::get_unplayed_normalized_video_ids(&pool, playlist)
            .await
            .unwrap(),
        vec![ids[1], ids[2]]
    );
}

/// The rotation's end: the blocked song is the one never played, and the
/// rotation starts over from the others rather than pick it.
#[tokio::test]
async fn a_rotation_starts_over_rather_than_pick_a_blocked_song() {
    let pool = memory_pool().await;
    let (playlist, ids) = songs(&pool, "fast", &[0, 1, 2]).await;
    models::record_play(&pool, playlist, ids[0]).await.unwrap();
    models::record_play(&pool, playlist, ids[1]).await.unwrap();
    block_yt(&pool, 2).await;
    for _ in 0..10 {
        let pick = VideoSelector::select_next(&pool, playlist, PlaybackMode::Single, None, &[])
            .await
            .unwrap()
            .expect("a song not blocked");
        assert_ne!(pick, ids[2]);
    }
}

/// Loop after a restart does not take a remembered song blocked since; a
/// Loop with the song in memory (not a pick) keeps playing it.
#[tokio::test]
async fn loop_after_a_restart_skips_a_blocked_remembered_song() {
    let pool = memory_pool().await;
    let (playlist, ids) = songs(&pool, "fast", &[0, 1, 2]).await;
    loop_memory::remember(&pool, playlist, ids[1])
        .await
        .unwrap();
    block_yt(&pool, 1).await;
    assert_eq!(
        loop_memory::remembered(&pool, playlist).await.unwrap(),
        None
    );
    for _ in 0..10 {
        let pick = VideoSelector::select_next(&pool, playlist, PlaybackMode::Loop, None, &[])
            .await
            .unwrap()
            .expect("a song not blocked");
        assert_ne!(pick, ids[1]);
    }
    let held = VideoSelector::select_next(&pool, playlist, PlaybackMode::Loop, Some(ids[1]), &[])
        .await
        .unwrap();
    assert_eq!(held, Some(ids[1]), "the song in memory plays on");
}

/// An operator's set list plays a blocked song: the block stops automatic
/// picks only.
#[tokio::test]
async fn a_custom_set_list_keeps_a_blocked_song() {
    let pool = memory_pool().await;
    let (_, ids) = songs(&pool, "fast", &[0, 1]).await;
    let custom: i64 = sqlx::query_scalar(
        "INSERT INTO playlists (name, youtube_url, kind) VALUES ('live', '', 'custom') \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    models::append_playlist_item(&pool, custom, ids[1])
        .await
        .unwrap();
    block_yt(&pool, 1).await;
    loop_memory::remember(&pool, custom, ids[1]).await.unwrap();
    assert_eq!(
        loop_memory::remembered(&pool, custom).await.unwrap(),
        Some(ids[1])
    );
    let pick = VideoSelector::select_next(&pool, custom, PlaybackMode::Continuous, None, &[])
        .await
        .unwrap();
    assert_eq!(pick, Some(ids[1]));
}
