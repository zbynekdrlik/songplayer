//! V28 migration tests (#184 round G1) — the ONE mixer console gains a per-KIND
//! memory. V28 derives the SONG pair (`mix_song_vokaly` / `mix_song_podklad`) from
//! V27's single `mix_vokaly` / `mix_podklad`, seeds the DUB triple
//! (`mix_dub_vokaly` / `mix_dub_podklad` / `mix_dub_dabing`) with the `Len dabing`
//! default `(0, 1, 1)`, and deletes the three old global keys. Sibling of
//! `mod_tests_v27.rs`; uses the shared `apply_first_n` / `apply_upto` so V28 fires
//! in isolation from any later migration.

use super::MIGRATIONS;
use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

async fn get_setting(pool: &SqlitePool, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn migration_v28_derives_song_pair_and_seeds_dub_memory() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 27).await;
    // V27 seeded the globals from (absent) karaoke; overwrite with the operator's
    // last global mix so the SONG derivation is exercised on real values.
    for (k, v) in [
        ("mix_vokaly", "0.4"),
        ("mix_podklad", "1"),
        ("mix_dabing", "1"),
    ] {
        sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
            .bind(k)
            .bind(v)
            .execute(&pool)
            .await
            .unwrap();
    }

    apply_upto(&pool, 28).await;

    // The SONG pair is derived from the old globals.
    assert_eq!(
        get_setting(&pool, "mix_song_vokaly").await,
        Some("0.4".into())
    );
    assert_eq!(
        get_setting(&pool, "mix_song_podklad").await,
        Some("1".into())
    );
    // The DUB triple is seeded to Len dabing (0, 1, 1) — dub-only by default.
    assert_eq!(get_setting(&pool, "mix_dub_vokaly").await, Some("0".into()));
    assert_eq!(
        get_setting(&pool, "mix_dub_podklad").await,
        Some("1".into())
    );
    assert_eq!(get_setting(&pool, "mix_dub_dabing").await, Some("1".into()));
    // The three old global keys are gone.
    assert_eq!(get_setting(&pool, "mix_vokaly").await, None);
    assert_eq!(get_setting(&pool, "mix_podklad").await, None);
    assert_eq!(get_setting(&pool, "mix_dabing").await, None);
}

#[tokio::test]
async fn migration_v28_song_pair_falls_back_to_full_when_globals_absent() {
    // A DB where the globals were never set → the COALESCE fallback seeds song (1,1).
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 27).await;
    sqlx::query("DELETE FROM settings WHERE key IN ('mix_vokaly', 'mix_podklad', 'mix_dabing')")
        .execute(&pool)
        .await
        .unwrap();

    apply_upto(&pool, 28).await;

    assert_eq!(
        get_setting(&pool, "mix_song_vokaly").await,
        Some("1".into())
    );
    assert_eq!(
        get_setting(&pool, "mix_song_podklad").await,
        Some("1".into())
    );
    // The dub seed is a fixed default, independent of the globals.
    assert_eq!(get_setting(&pool, "mix_dub_vokaly").await, Some("0".into()));
    assert_eq!(
        get_setting(&pool, "mix_dub_podklad").await,
        Some("1".into())
    );
    assert_eq!(get_setting(&pool, "mix_dub_dabing").await, Some("1".into()));
}

#[tokio::test]
async fn migration_v28_advances_schema_version() {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    let v = current_schema_version(&pool).await.unwrap();
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 28, "V28 must be part of the migration list");
    assert_eq!(v, latest);
}
