//! #229 `db::models_peer` — the node exchange's sha256 cache.

use std::collections::HashSet;

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
