//! #136: the metadata health record behind `status.metadata`.

use super::*;
use crate::db;

fn names() -> Vec<String> {
    vec!["claude".to_string(), "gemini".to_string()]
}

#[test]
fn a_new_record_names_every_provider_in_chain_order_with_nothing_seen() {
    let health = MetadataHealth::new(names());

    assert_eq!(
        health.snapshot(),
        [
            ProviderHealth {
                name: "claude".into(),
                last_ok_at_ms: None,
                last_error: None
            },
            ProviderHealth {
                name: "gemini".into(),
                last_ok_at_ms: None,
                last_error: None
            },
        ]
    );
}

#[test]
fn each_record_touches_only_its_own_provider() {
    let health = MetadataHealth::new(names());

    health.record_error(1, "gemini: all 5 keys failed");
    health.record_ok(0, 1_000);

    let snap = health.snapshot();
    assert_eq!(snap[0].last_ok_at_ms, Some(1_000));
    assert_eq!(snap[0].last_error, None);
    assert_eq!(snap[1].last_ok_at_ms, None);
    assert_eq!(
        snap[1].last_error.as_deref(),
        Some("gemini: all 5 keys failed")
    );

    // An answer keeps the time and clears the error; a later error keeps the
    // time of the last answer.
    health.record_ok(1, 2_000);
    health.record_error(1, "rate limited");
    let snap = health.snapshot();
    assert_eq!(snap[1].last_ok_at_ms, Some(2_000));
    assert_eq!(snap[1].last_error.as_deref(), Some("rate limited"));
}

#[test]
fn an_index_outside_the_chain_is_ignored() {
    let health = MetadataHealth::new(names());

    health.record_ok(2, 5);
    health.record_error(7, "nobody");

    assert!(
        health
            .snapshot()
            .iter()
            .all(|p| p.last_ok_at_ms.is_none() && p.last_error.is_none())
    );
}

#[test]
fn a_recorded_error_is_cut_to_the_bound() {
    let health = MetadataHealth::new(names());
    let long = "é".repeat(MAX_ERROR_CHARS + 50);

    health.record_error(0, &long);

    let kept = health.snapshot()[0].last_error.clone().unwrap();
    assert_eq!(kept.chars().count(), MAX_ERROR_CHARS);
    assert_eq!(bounded_error("short"), "short");
}

#[tokio::test]
async fn failed_videos_counts_exactly_the_repair_queue() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'url')")
        .execute(&pool)
        .await
        .unwrap();
    // (youtube_id, gemini_failed, normalized): two in the queue, one failed
    // but not yet normalized, one fine.
    for (yt, gf, norm) in [("a1", 1, 1), ("a2", 1, 1), ("a3", 1, 0), ("a4", 0, 1)] {
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, gemini_failed, normalized)
             VALUES (1, ?, 't', ?, ?)",
        )
        .bind(yt)
        .bind(gf)
        .bind(norm)
        .execute(&pool)
        .await
        .unwrap();
    }

    assert_eq!(failed_videos(&pool).await.unwrap(), 2);
}

/// #136: a row whose title the operator corrected (`metadata_source =
/// 'manual'`) is never in the repair queue, even with `gemini_failed = 1`
/// left over: the repair would write over the correction.
#[tokio::test]
async fn a_manual_row_is_not_in_the_repair_queue() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'url')")
        .execute(&pool)
        .await
        .unwrap();
    // (youtube_id, metadata_source): parser-named, never named, and corrected.
    for (yt, source) in [("b1", Some("regex")), ("b2", None), ("b3", Some("manual"))] {
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, gemini_failed, normalized, \
                                 metadata_source)
             VALUES (1, ?, 't', 1, 1, ?)",
        )
        .bind(yt)
        .bind(source)
        .execute(&pool)
        .await
        .unwrap();
    }

    assert_eq!(failed_videos(&pool).await.unwrap(), 2);
}
