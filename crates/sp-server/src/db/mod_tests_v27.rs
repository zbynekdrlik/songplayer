//! V27 migration tests (#184 round G) — the ONE mixer console derives its three
//! fader settings from the old karaoke settings, deletes those keys, and drops
//! the per-video `dub_mix_ratio` column. Sibling file split from `mod_tests.rs`
//! for the 1000-line cap; uses the shared `apply_first_n` / `column_names`.

use super::MIGRATIONS;
use super::test_helpers::{apply_first_n, column_names};
use super::*;

async fn get_setting(pool: &SqlitePool, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
        .unwrap()
}

/// Apply V1..=26, seed the old karaoke settings + a video, fire V27, and return
/// the three derived fader settings.
async fn derive(
    mode: Option<&str>,
    vg: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>) {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 26).await;
    if let Some(m) = mode {
        sqlx::query("INSERT INTO settings (key, value) VALUES ('karaoke_mode', ?)")
            .bind(m)
            .execute(&pool)
            .await
            .unwrap();
    }
    if let Some(g) = vg {
        sqlx::query("INSERT INTO settings (key, value) VALUES ('karaoke_vocal_gain', ?)")
            .bind(g)
            .execute(&pool)
            .await
            .unwrap();
    }
    run_migrations(&pool).await.unwrap();
    (
        get_setting(&pool, "mix_vokaly").await,
        get_setting(&pool, "mix_podklad").await,
        get_setting(&pool, "mix_dabing").await,
    )
}

#[tokio::test]
async fn migration_v27_derivation_table() {
    // karaoke_low + vg=0.4 → (0.4, 1, 1).
    assert_eq!(
        derive(Some("karaoke_low"), Some("0.4")).await,
        (Some("0.4".into()), Some("1".into()), Some("1".into()))
    );
    // full_mix → (1, 1, 1).
    assert_eq!(
        derive(Some("full_mix"), None).await,
        (Some("1".into()), Some("1".into()), Some("1".into()))
    );
    // vocals_only → (1, 0, 1).
    assert_eq!(
        derive(Some("vocals_only"), None).await,
        (Some("1".into()), Some("0".into()), Some("1".into()))
    );
    // instrumental_only → (0, 1, 1).
    assert_eq!(
        derive(Some("instrumental_only"), None).await,
        (Some("0".into()), Some("1".into()), Some("1".into()))
    );
    // No karaoke_mode at all → the full default (1, 1, 1).
    assert_eq!(
        derive(None, None).await,
        (Some("1".into()), Some("1".into()), Some("1".into()))
    );
    // karaoke_low with NO stored vocal gain → the 0.3 default for vokaly.
    assert_eq!(
        derive(Some("karaoke_low"), None).await,
        (Some("0.3".into()), Some("1".into()), Some("1".into()))
    );
}

#[tokio::test]
async fn migration_v27_deletes_old_keys_and_drops_the_column() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 26).await;
    sqlx::query("INSERT INTO settings (key, value) VALUES ('karaoke_mode', 'instrumental_only')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO settings (key, value) VALUES ('karaoke_vocal_gain', '0.2')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    // A video row exists (with the V26 dub_mix_ratio column, default 1.0).
    sqlx::query("INSERT INTO videos (playlist_id, youtube_id, title) VALUES (1, 'aaa', 't')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        column_names(&pool, "videos")
            .await
            .contains(&"dub_mix_ratio".to_string())
    );

    run_migrations(&pool).await.unwrap();

    // The two old keys are gone.
    assert_eq!(get_setting(&pool, "karaoke_mode").await, None);
    assert_eq!(get_setting(&pool, "karaoke_vocal_gain").await, None);
    // The per-video ratio column is dropped.
    assert!(
        !column_names(&pool, "videos")
            .await
            .contains(&"dub_mix_ratio".to_string()),
        "V27 must DROP the dub_mix_ratio column"
    );
}

#[tokio::test]
async fn migration_v27_advances_schema_version() {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    let v = current_schema_version(&pool).await.unwrap();
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 27, "V27 must be part of the migration list");
    assert_eq!(v, latest);
}
