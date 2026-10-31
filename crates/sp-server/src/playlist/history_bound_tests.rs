//! #261: `play_history` keeps one row per playlist and song, and nothing
//! older than 30 days outside a youtube / dabing playlist's rotation.

use sqlx::SqlitePool;

use super::{Bounded, KEEP_DAYS, cutoff_s, pass};

/// 9.10.2025 08:53:20.456 UTC: not a whole second, so the cutoff's
/// truncation shows.
const NOW_MS: i64 = 1_760_000_000_456;
const DAY_S: i64 = 86_400;

#[test]
fn the_cutoff_is_30_days_before_now_in_whole_seconds() {
    assert_eq!(cutoff_s(NOW_MS), 1_757_408_000);
    assert_eq!(cutoff_s(NOW_MS), NOW_MS / 1000 - KEEP_DAYS * DAY_S);
}

/// Playlists 1 (youtube: songs 11, 12), 2 (dabing: 21), 3 (custom: a set
/// list, no songs of its own) and 4 (youtube: 41).
async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, kind) VALUES \
         (1, 'fast', 'u1', 'youtube'), (2, 'dub', 'u2', 'dabing'), \
         (3, 'live', '', 'custom'), (4, 'slow', 'u4', 'youtube')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id) VALUES \
         (11, 1, 'aaaaaaaaaaa'), (12, 1, 'bbbbbbbbbbb'), (21, 2, 'ccccccccccc'), \
         (41, 4, 'ddddddddddd')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// A play `id` of `video` on `playlist` at unix second `at_s`.
async fn play(pool: &SqlitePool, id: i64, playlist: i64, video: i64, at_s: i64) {
    sqlx::query(
        "INSERT INTO play_history (id, playlist_id, video_id, played_at) \
         VALUES (?, ?, ?, datetime(?, 'unixepoch'))",
    )
    .bind(id)
    .bind(playlist)
    .bind(video)
    .bind(at_s)
    .execute(pool)
    .await
    .unwrap();
}

async fn ids(pool: &SqlitePool) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM play_history ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Every case of the two rules on one table:
///
/// - 1 / 2: a youtube playlist's own song twice, both old: the older play
///   goes as a duplicate, the newer stays (its rotation);
/// - 3: a youtube playlist's own song, 40 days old: its rotation, kept;
/// - 4: a dabing playlist's own song, 40 days old: kept;
/// - 5 / 6: ytlive's play of a song, old, then again yesterday: the old
///   one goes as a duplicate;
/// - 7: ytlive's play at the cutoff itself: kept; 8: one second before
///   it: old;
/// - 9: a youtube playlist's play of ANOTHER playlist's song, old: no
///   rotation of it, old;
/// - 10 / 11: the same playlist, song and second twice: the lower id goes.
#[tokio::test]
async fn a_pass_keeps_one_play_per_song_and_a_rotation_of_any_age() {
    let pool = pool().await;
    let now_s = NOW_MS / 1000;
    let cutoff = cutoff_s(NOW_MS);
    play(&pool, 1, 1, 11, now_s - 40 * DAY_S).await;
    play(&pool, 2, 1, 11, now_s - 35 * DAY_S).await;
    play(&pool, 3, 1, 12, now_s - 40 * DAY_S).await;
    play(&pool, 4, 2, 21, now_s - 40 * DAY_S).await;
    play(&pool, 5, 3, 11, now_s - 40 * DAY_S).await;
    play(&pool, 6, 3, 11, now_s - DAY_S).await;
    play(&pool, 7, 3, 12, cutoff).await;
    play(&pool, 8, 3, 41, cutoff - 1).await;
    play(&pool, 9, 1, 41, now_s - 40 * DAY_S).await;
    play(&pool, 10, 4, 41, now_s - 2 * DAY_S).await;
    play(&pool, 11, 4, 41, now_s - 2 * DAY_S).await;

    assert_eq!(
        pass(&pool, NOW_MS).await.unwrap(),
        Bounded {
            duplicates: 3,
            old: 2,
        }
    );
    assert_eq!(ids(&pool).await, [2, 3, 4, 6, 7, 11]);
    assert_eq!(
        pass(&pool, NOW_MS).await.unwrap(),
        Bounded::default(),
        "a second pass finds nothing"
    );
}

/// The rotation still reads the same after a pass: a song played on ytlive
/// is unplayed at home, the home's own plays are kept whatever their age.
#[tokio::test]
async fn a_pass_leaves_the_home_rotation_as_it_was() {
    let pool = pool().await;
    let now_s = NOW_MS / 1000;
    sqlx::query("UPDATE videos SET normalized = 1")
        .execute(&pool)
        .await
        .unwrap();
    play(&pool, 1, 1, 12, now_s - 200 * DAY_S).await;
    play(&pool, 2, 3, 11, now_s - 200 * DAY_S).await;
    let unplayed = crate::db::models::get_unplayed_normalized_video_ids(&pool, 1)
        .await
        .unwrap();
    assert_eq!(unplayed, [11]);
    pass(&pool, NOW_MS).await.unwrap();
    assert_eq!(ids(&pool).await, [1]);
    let unplayed = crate::db::models::get_unplayed_normalized_video_ids(&pool, 1)
        .await
        .unwrap();
    assert_eq!(unplayed, [11]);
}
