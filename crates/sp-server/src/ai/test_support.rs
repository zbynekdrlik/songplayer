//! #321: the text-AI client the call sites' tests drive against a mock
//! OpenRouter (the OpenAI chat shape on `{api_url}/chat/completions`).

use sqlx::SqlitePool;

use crate::ai::client::AiClient;
use crate::ai::openrouter::OpenRouterClient;
use crate::ai::retry::RetryPolicy;

/// The key the test clients send.
pub const TEST_KEY: &str = "example-openrouter-key-for-tests";

/// A client on `api_url` (a mock server's root, e.g. `{uri}/v1`) with its
/// own in-memory database holding the key (paid AI on: unset), and no real
/// waits between retries.
pub async fn ai_client_at(api_url: impl Into<String>) -> AiClient {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    ai_client_on(pool, api_url).await
}

/// A client on `api_url` reading `pool` (the key is set in it).
pub async fn ai_client_on(pool: SqlitePool, api_url: impl Into<String>) -> AiClient {
    crate::db::models::set_setting(
        &pool,
        sp_core::ai_tasks::SETTING_OPENROUTER_API_KEY,
        TEST_KEY,
    )
    .await
    .unwrap();
    AiClient::with_openrouter(pool, OpenRouterClient::new(api_url))
        .with_retry_policy(RetryPolicy::NO_WAIT)
}

/// A client for a test that never calls it: nothing listens on its root.
pub async fn ai_client_unused() -> AiClient {
    ai_client_at("http://127.0.0.1:9/v1").await
}
