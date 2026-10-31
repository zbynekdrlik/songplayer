//! #260: a fetch marks the songs that left the YouTube playlist and brings
//! back the ones listed again.

use std::time::Duration;

use sqlx::SqlitePool;

use super::{Diff, SyncReport, diff, names, record_fetch};
use crate::db::models_membership::Member;
use crate::playlist::PlaylistEntry;

fn member(youtube_id: &str, title: Option<&str>, removed: bool) -> Member {
    Member {
        youtube_id: youtube_id.to_string(),
        title: title.map(str::to_string),
        removed,
    }
}

fn entry(id: &str, title: &str) -> PlaylistEntry {
    PlaylistEntry {
        id: id.to_string(),
        title: Some(title.to_string()),
        duration_ms: None,
    }
}

/// Every case at once: listed and in the list (no change), listed and not
/// (marked), marked and listed (back), marked and not (keeps its mark, in
/// neither list).
#[test]
fn a_fetch_marks_the_rows_it_misses_and_brings_back_the_ones_it_lists() {
    let rows = [
        member("in", Some("In"), false),
        member("gone", Some("Gone"), false),
        member("back", Some("Back"), true),
        member("still-gone", None, true),
    ];
    assert_eq!(
        diff(&rows, &["in", "back", "brand-new"]),
        Diff {
            marked: vec![member("gone", Some("Gone"), false)],
            back: vec![member("back", Some("Back"), true)],
        }
    );
}

#[test]
fn a_fetch_that_lists_every_row_changes_nothing() {
    let rows = [member("a", None, false), member("b", None, false)];
    assert_eq!(diff(&rows, &["b", "a"]), Diff::default());
}

#[test]
fn the_info_line_names_each_song_by_id_and_title() {
    assert_eq!(names(&[]), "-");
    assert_eq!(
        names(&[
            member("TwzfEwsTfag", Some("Never Lost"), false),
            member("CwDiynYEBDo", None, false),
        ]),
        "TwzfEwsTfag Never Lost; CwDiynYEBDo"
    );
}

/// Playlist 1 with rows `a`, `b`, `c` (`c` marked at 1 000), and playlist 2
/// with its own row of `b`.
async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p1', 'u1'), (2, 'p2', 'u2')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, removed_at) VALUES \
         (1, 'a', 'Song A', NULL), (1, 'b', 'Song B', NULL), (1, 'c', 'Song C', 1000), \
         (2, 'b', 'Song B', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

async fn removed_at(pool: &SqlitePool, playlist_id: i64, youtube_id: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT removed_at FROM videos WHERE playlist_id = ? AND youtube_id = ?")
        .bind(playlist_id)
        .bind(youtube_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The owner's case (13.10.2026): a song taken out of the YouTube playlist
/// is marked at the sync's time, a song put back is in it again, a new one
/// is a new row. Playlist 2's row of the same video is untouched.
#[tokio::test]
async fn a_sync_marks_the_songs_that_left_and_brings_back_the_ones_put_back() {
    let pool = pool().await;
    let report = record_fetch(
        &pool,
        1,
        &[
            entry("a", "Song A"),
            entry("c", "Song C"),
            entry("d", "Song D"),
        ],
        50_000,
    )
    .await
    .unwrap();
    assert_eq!(
        report,
        SyncReport {
            new: 1,
            marked: vec![member("b", Some("Song B"), false)],
            back: vec![member("c", Some("Song C"), true)],
        }
    );
    assert_eq!(removed_at(&pool, 1, "a").await, None);
    assert_eq!(removed_at(&pool, 1, "b").await, Some(50_000));
    assert_eq!(removed_at(&pool, 1, "c").await, None, "put back");
    assert_eq!(removed_at(&pool, 1, "d").await, None, "new");
    assert_eq!(removed_at(&pool, 2, "b").await, None, "another playlist");
}

/// A second sync that still misses the song keeps the first mark (the
/// retention's year counts from it) and reports nothing new.
#[tokio::test]
async fn a_song_still_missing_keeps_its_first_mark() {
    let pool = pool().await;
    let fetched = [entry("a", "Song A")];
    record_fetch(&pool, 1, &fetched, 50_000).await.unwrap();
    let again = record_fetch(&pool, 1, &fetched, 90_000).await.unwrap();
    assert_eq!(again, SyncReport::default());
    assert_eq!(removed_at(&pool, 1, "b").await, Some(50_000));
    assert_eq!(removed_at(&pool, 1, "c").await, Some(1_000));
}

/// yt-dlp can answer success with no entry: an empty fetch never empties
/// the playlist.
#[tokio::test]
async fn an_empty_fetch_marks_nothing() {
    let pool = pool().await;
    let report = record_fetch(&pool, 1, &[], 50_000).await.unwrap();
    assert_eq!(report, SyncReport::default());
    assert_eq!(removed_at(&pool, 1, "a").await, None);
    assert_eq!(removed_at(&pool, 1, "b").await, None);
    assert_eq!(removed_at(&pool, 1, "c").await, Some(1_000));
}

/// #261: a fetch of an inactive playlist (a manual sync) records nothing:
/// its upsert would clear the marks the deactivation set, and a new row
/// would escape the clock. An unknown playlist is no active one either.
#[tokio::test]
async fn a_fetch_of_an_inactive_playlist_records_nothing() {
    let pool = pool().await;
    sqlx::query("UPDATE playlists SET is_active = 0 WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let fetched = [
        entry("a", "Song A"),
        entry("c", "Song C"),
        entry("d", "Song D"),
    ];
    for playlist in [1, 99] {
        let report = record_fetch(&pool, playlist, &fetched, 50_000)
            .await
            .unwrap();
        assert_eq!(report, SyncReport::default());
    }
    assert_eq!(removed_at(&pool, 1, "b").await, None, "not marked");
    assert_eq!(removed_at(&pool, 1, "c").await, Some(1_000), "not back");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM videos")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 4, "no new row");
}

/// The retention holds `cache::SONG_FILES` from its last read of a row to
/// its deletion; a sync writes under the same lock, so it never brings a
/// song back while that song's files are being deleted.
#[tokio::test]
async fn a_sync_writes_under_the_song_files_lock() {
    let pool = pool().await;
    let held = crate::downloader::cache::SONG_FILES.lock().await;
    let sync_pool = pool.clone();
    let sync = tokio::spawn(async move {
        record_fetch(&sync_pool, 1, &[entry("a", "Song A")], 50_000)
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!sync.is_finished(), "the sync waits for the lock");
    assert_eq!(removed_at(&pool, 1, "b").await, None, "nothing written yet");
    drop(held);
    let report = tokio::time::timeout(Duration::from_secs(60), sync)
        .await
        .expect("the sync finishes once the lock is free")
        .unwrap();
    assert_eq!(report.marked.len(), 1);
    assert_eq!(removed_at(&pool, 1, "b").await, Some(50_000));
}
