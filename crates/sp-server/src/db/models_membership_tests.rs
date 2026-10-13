//! #260: the V40 mark's queries.

use sqlx::SqlitePool;

use super::{Member, mark_removed, members, video_playlist_membership};

/// Playlist 1 with rows 11 (`a`, titled), 12 (`b`, no title) and 13 (`c`,
/// not downloaded); playlist 2 with row 21, the same video `a`.
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
        "INSERT INTO videos (id, playlist_id, youtube_id, title, normalized) VALUES \
         (11, 1, 'a', 'Song A', 1), (12, 1, 'b', NULL, 1), (13, 1, 'c', 'Song C', 0), \
         (21, 2, 'a', 'Song A', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

async fn removed_at(pool: &SqlitePool, id: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT removed_at FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn member(youtube_id: &str, title: Option<&str>, removed: bool) -> Member {
    Member {
        youtube_id: youtube_id.to_string(),
        title: title.map(str::to_string),
        removed,
    }
}

#[tokio::test]
async fn members_are_the_playlists_rows_in_id_order_with_their_marks() {
    let pool = pool().await;
    mark_removed(&pool, 1, "b", 5_000).await.unwrap();
    assert_eq!(
        members(&pool, 1).await.unwrap(),
        vec![
            member("a", Some("Song A"), false),
            member("b", None, true),
            member("c", Some("Song C"), false),
        ]
    );
    assert_eq!(
        members(&pool, 2).await.unwrap(),
        vec![member("a", Some("Song A"), false)]
    );
}

/// The mark is per row: the same video in another playlist stays in it.
#[tokio::test]
async fn a_mark_is_the_one_rows() {
    let pool = pool().await;
    mark_removed(&pool, 1, "a", 7_000).await.unwrap();
    assert_eq!(removed_at(&pool, 11).await, Some(7_000));
    assert_eq!(removed_at(&pool, 21).await, None, "playlist 2's row");
    assert_eq!(removed_at(&pool, 12).await, None, "another video");
}

/// The retention's year counts from the day the song left: a later sync
/// that still misses it never moves the mark.
#[tokio::test]
async fn a_row_keeps_its_first_mark() {
    let pool = pool().await;
    mark_removed(&pool, 1, "a", 7_000).await.unwrap();
    mark_removed(&pool, 1, "a", 9_000).await.unwrap();
    assert_eq!(removed_at(&pool, 11).await, Some(7_000));
}

/// A ▶ of a song by id on a youtube playlist: a marked row is not in the
/// playlist any more (the dashboard no longer lists it either).
#[tokio::test]
async fn a_marked_row_is_not_a_member_of_its_playlist() {
    let pool = pool().await;
    assert_eq!(
        video_playlist_membership(&pool, 1, 11).await.unwrap(),
        Some(true)
    );
    assert_eq!(
        video_playlist_membership(&pool, 1, 13).await.unwrap(),
        Some(false),
        "not downloaded"
    );
    mark_removed(&pool, 1, "a", 7_000).await.unwrap();
    assert_eq!(video_playlist_membership(&pool, 1, 11).await.unwrap(), None);
    assert_eq!(
        video_playlist_membership(&pool, 2, 21).await.unwrap(),
        Some(true),
        "the same video in playlist 2"
    );
}
