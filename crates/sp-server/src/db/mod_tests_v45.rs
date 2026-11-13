//! V45 (#321): the text AI's unread settings rows deleted — `ai_model`,
//! `ai_api_url` and every `ai_provider_<task>` — while each task's OpenRouter
//! model (`ai_model_<task>`) and every other setting stay.

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

/// Every settings key, sorted.
async fn keys(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar("SELECT key FROM settings ORDER BY key")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// A box at V44 holding the retired rows next to the rows that stay.
async fn box_at_v44() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 44).await;
    sqlx::query("DELETE FROM settings")
        .execute(&pool)
        .await
        .unwrap();
    for (key, value) in [
        ("ai_model", "claude-opus-5-5"),
        ("ai_api_url", "http://127.0.0.1:18787/v1"),
        ("ai_provider_translate", "claude_proxy"),
        ("ai_provider_register", "openrouter"),
        ("ai_provider_spotify", ""),
        ("ai_model_translate", "google/gemini-4-carbon"),
        ("ai_model_register", ""),
        ("ai_models", "x"),
        ("openrouter_api_key", "example"),
        ("gemini_model", "m"),
    ] {
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool
}

#[tokio::test]
async fn v45_deletes_the_unread_text_ai_rows_and_keeps_the_rest() {
    let pool = box_at_v44().await;
    apply_upto(&pool, 45).await;
    assert_eq!(
        keys(&pool).await,
        [
            "ai_model_register",
            "ai_model_translate",
            "ai_models",
            "gemini_model",
            "openrouter_api_key",
        ]
    );
    let model: String =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'ai_model_translate'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        model, "google/gemini-4-carbon",
        "a task's model is kept as stored"
    );
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 45, "{latest}");
}

/// A box with none of the rows migrates clean (nothing to delete).
#[tokio::test]
async fn v45_on_a_box_without_the_rows_changes_nothing() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 44).await;
    let before = keys(&pool).await;
    apply_upto(&pool, 45).await;
    assert_eq!(keys(&pool).await, before);
}
