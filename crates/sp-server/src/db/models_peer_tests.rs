//! #229 `db::models_peer` — the node exchange's sha256 cache, waits,
//! provenance and job defers.

use std::collections::HashSet;
use std::time::Duration;

use sqlx::SqlitePool;

use super::*;

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

fn entry(path: &str, sha: &str) -> HashEntry {
    HashEntry {
        path: path.into(),
        size: 3,
        mtime_ms: 4,
        sha256: sha.into(),
        hashed_at_ms: 5,
    }
}

#[tokio::test]
async fn a_hash_is_stored_replaced_and_removed_by_path() {
    let pool = pool().await;
    put_hash(&pool, &entry("/c/a", "s1")).await.unwrap();
    put_hash(&pool, &entry("/c/b", "s3")).await.unwrap();
    let replaced = HashEntry {
        size: 9,
        mtime_ms: 8,
        hashed_at_ms: 7,
        ..entry("/c/a", "s2")
    };
    put_hash(&pool, &replaced).await.unwrap();
    let all = all_hashes(&pool).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all["/c/a"], replaced, "every field of the newer entry");
    assert_eq!(all["/c/b"], entry("/c/b", "s3"), "the other path untouched");
    assert_eq!(remove_hash(&pool, "/c/a").await.unwrap(), 1);
    assert_eq!(remove_hash(&pool, "/c/a").await.unwrap(), 0);
    let left: Vec<String> = all_hashes(&pool).await.unwrap().into_keys().collect();
    assert_eq!(left, vec!["/c/b".to_string()]);
}

#[tokio::test]
async fn prune_keeps_only_the_named_paths() {
    let pool = pool().await;
    for p in ["/c/a", "/c/b", "/c/c"] {
        put_hash(&pool, &entry(p, "s")).await.unwrap();
    }
    let keep: HashSet<String> = ["/c/b".to_string()].into_iter().collect();
    assert_eq!(prune_hashes(&pool, &keep).await.unwrap(), 2);
    let left: Vec<String> = all_hashes(&pool).await.unwrap().into_keys().collect();
    assert_eq!(left, vec!["/c/b".to_string()]);
    assert_eq!(
        prune_hashes(&pool, &keep).await.unwrap(),
        0,
        "nothing left to prune"
    );
}

#[tokio::test]
async fn a_wait_keeps_its_first_start_until_it_ends() {
    let pool = pool().await;
    assert_eq!(
        waited(&pool, "aaaaaaaaaaa", "stems", 5_000).await.unwrap(),
        None
    );
    start_wait(&pool, "aaaaaaaaaaa", "stems", 1_000)
        .await
        .unwrap();
    start_wait(&pool, "aaaaaaaaaaa", "stems", 4_000)
        .await
        .unwrap();
    assert_eq!(
        waited(&pool, "aaaaaaaaaaa", "stems", 61_000).await.unwrap(),
        Some(Duration::from_secs(60)),
        "counted from the first start"
    );
    assert_eq!(
        waited(&pool, "aaaaaaaaaaa", "lyrics", 61_000)
            .await
            .unwrap(),
        None,
        "per job"
    );
    assert_eq!(
        waited(&pool, "bbbbbbbbbbb", "stems", 61_000).await.unwrap(),
        None,
        "per video"
    );
    assert_eq!(
        waited(&pool, "aaaaaaaaaaa", "stems", 500).await.unwrap(),
        Some(Duration::ZERO),
        "a clock stepped back"
    );
    end_wait(&pool, "bbbbbbbbbbb", "stems").await.unwrap();
    assert!(
        waited(&pool, "aaaaaaaaaaa", "stems", 61_000)
            .await
            .unwrap()
            .is_some(),
        "another video's end leaves this wait"
    );
    end_wait(&pool, "aaaaaaaaaaa", "stems").await.unwrap();
    assert_eq!(
        waited(&pool, "aaaaaaaaaaa", "stems", 61_000).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn a_fetch_record_is_kept_per_video_and_kind() {
    let pool = pool().await;
    record_fetch(&pool, "aaaaaaaaaaa", "audio", "snv", 1, "s1", 10)
        .await
        .unwrap();
    record_fetch(&pool, "aaaaaaaaaaa", "audio", "snv", 3, "s2", 20)
        .await
        .unwrap();
    record_fetch(&pool, "aaaaaaaaaaa", "video", "pp", 2, "s3", 30)
        .await
        .unwrap();
    assert_eq!(
        fetch_record(&pool, "aaaaaaaaaaa", "audio").await.unwrap(),
        Some(("snv".to_string(), 3, "s2".to_string())),
        "the latest fetch wins"
    );
    assert_eq!(
        fetch_record(&pool, "aaaaaaaaaaa", "video").await.unwrap(),
        Some(("pp".to_string(), 2, "s3".to_string()))
    );
    assert_eq!(
        fetch_record(&pool, "aaaaaaaaaaa", "lyrics").await.unwrap(),
        None
    );
    let at: i64 = sqlx::query_scalar(
        "SELECT fetched_at_ms FROM peer_fetches WHERE youtube_id = 'aaaaaaaaaaa' AND kind = 'audio'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(at, 20);
}

#[tokio::test]
async fn a_job_defer_sets_only_its_recheck() {
    let pool = pool().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, download_attempts, stem_attempts) \
         VALUES (1, 'aaaaaaaaaaa', 0, 2, 3) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let other: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, normalized) \
         VALUES (1, 'bbbbbbbbbbb', 0) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let before = chrono::Utc::now();
    defer_download(&pool, id, Duration::from_secs(120))
        .await
        .unwrap();
    defer_stems(&pool, id, Duration::from_secs(300))
        .await
        .unwrap();
    type Deferred = (String, i64, String, i64, Option<String>);
    let (next, attempts, stem_next, stem_attempts, stem_status): Deferred = sqlx::query_as(
        "SELECT next_attempt_at, download_attempts, stem_next_attempt_at, stem_attempts, \
                stem_status FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let next = chrono::DateTime::parse_from_rfc3339(&next).unwrap();
    let ahead = (next.with_timezone(&chrono::Utc) - before).num_seconds();
    assert!((119..=125).contains(&ahead), "{ahead}");
    assert_eq!((attempts, stem_attempts, stem_status), (2, 3, None));
    let stem_next = chrono::DateTime::parse_from_rfc3339(&stem_next).unwrap();
    let stem_ahead = (stem_next.with_timezone(&chrono::Utc) - before).num_seconds();
    assert!((299..=305).contains(&stem_ahead), "{stem_ahead}");
    let untouched: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT next_attempt_at, stem_next_attempt_at FROM videos WHERE id = ?")
            .bind(other)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(untouched, (None, None), "only the deferred row");
}
