//! #321: every task goes to OpenRouter with its model, read live.

use sp_core::ai_tasks::{AiTask, DEFAULT_OPENROUTER_MODEL, SETTING_OPENROUTER_API_KEY};
use sp_core::config::SETTING_PAID_AI_ENABLED;
use sqlx::SqlitePool;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::model_of;
use crate::ai::client::AiClient;
use crate::ai::openrouter::OpenRouterClient;
use crate::ai::retry::RetryPolicy;
use crate::db::models::set_setting;

const KEY: &str = "example-openrouter-key-for-tests";

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

fn answer(text: &str) -> serde_json::Value {
    serde_json::json!({"choices": [{"message": {"content": text}}]})
}

/// An OpenRouter mock that answers a request carrying the key, and a client
/// on it.
async fn rig(pool: &SqlitePool) -> (MockServer, AiClient) {
    let openrouter = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer("from openrouter")))
        .mount(&openrouter)
        .await;
    let client = AiClient::with_openrouter(
        pool.clone(),
        OpenRouterClient::new(format!("{}/api/v1", openrouter.uri())),
    )
    .with_retry_policy(RetryPolicy::NO_WAIT);
    (openrouter, client)
}

fn model_sent(server_requests: &[wiremock::Request]) -> Vec<String> {
    server_requests
        .iter()
        .map(|r| {
            #[derive(serde::Deserialize)]
            struct Body {
                model: String,
            }
            serde_json::from_slice::<Body>(&r.body).unwrap().model
        })
        .collect()
}

/// Nothing set: every task goes to OpenRouter with the newest Gemini — an
/// old `ai_provider_<task>` row naming the deleted proxy is not read (V45
/// deletes such rows; this one is written straight to the table).
#[tokio::test]
async fn with_nothing_set_every_task_goes_to_openrouter_with_the_default_model() {
    let pool = pool().await;
    let (openrouter, client) = rig(&pool).await;
    set_setting(&pool, SETTING_OPENROUTER_API_KEY, KEY)
        .await
        .unwrap();
    set_setting(&pool, "ai_provider_translate", "claude_proxy")
        .await
        .unwrap();
    for task in AiTask::ALL {
        assert_eq!(
            client.chat_for(task, "", "q", 10).await.unwrap(),
            "from openrouter"
        );
        assert_eq!(client.model(task).await, DEFAULT_OPENROUTER_MODEL);
    }
    assert_eq!(
        model_sent(&openrouter.received_requests().await.unwrap()),
        vec![DEFAULT_OPENROUTER_MODEL; 6]
    );
}

/// A task's model is read live (no restart), trimmed; blank again = the
/// default; another task keeps the default.
#[tokio::test]
async fn a_tasks_model_is_read_live() {
    let pool = pool().await;
    let (openrouter, client) = rig(&pool).await;
    set_setting(&pool, SETTING_OPENROUTER_API_KEY, KEY)
        .await
        .unwrap();
    set_setting(&pool, "ai_model_translate", " google/gemini-4-carbon ")
        .await
        .unwrap();
    for task in [AiTask::Translate, AiTask::Register] {
        client.chat_for(task, "", "q", 10).await.unwrap();
    }
    set_setting(&pool, "ai_model_translate", "").await.unwrap();
    client
        .chat_for(AiTask::Translate, "", "q", 10)
        .await
        .unwrap();
    assert_eq!(
        model_sent(&openrouter.received_requests().await.unwrap()),
        vec![
            "google/gemini-4-carbon",
            DEFAULT_OPENROUTER_MODEL,
            DEFAULT_OPENROUTER_MODEL
        ]
    );
}

/// A failed call names the task and the model.
#[tokio::test]
async fn a_failed_call_names_the_task_and_the_model() {
    let pool = pool().await;
    let openrouter = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
        .mount(&openrouter)
        .await;
    let client = AiClient::with_openrouter(
        pool.clone(),
        OpenRouterClient::new(format!("{}/api/v1", openrouter.uri())),
    );
    set_setting(&pool, SETTING_OPENROUTER_API_KEY, KEY)
        .await
        .unwrap();
    set_setting(&pool, "ai_model_spotify", "x/y").await.unwrap();
    let err = client
        .chat_for(AiTask::Spotify, "", "q", 10)
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("spotify via OpenRouter (x/y)"), "{text}");
    assert!(text.contains("HTTP 400"), "{text}");
}

/// Paid AI off: nothing is sent, and the error says why.
#[tokio::test]
async fn nothing_is_sent_while_paid_ai_is_off() {
    let pool = pool().await;
    let (openrouter, client) = rig(&pool).await;
    set_setting(&pool, SETTING_OPENROUTER_API_KEY, KEY)
        .await
        .unwrap();
    set_setting(&pool, SETTING_PAID_AI_ENABLED, "false")
        .await
        .unwrap();
    let err = client
        .chat_for(AiTask::Cleanup, "", "q", 10)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("paid AI is off"), "{err:#}");
    assert!(openrouter.received_requests().await.unwrap().is_empty());
}

/// No key: nothing sent, the error names the setting.
#[tokio::test]
async fn with_no_key_nothing_is_sent() {
    let pool = pool().await;
    let (openrouter, client) = rig(&pool).await;
    set_setting(&pool, SETTING_OPENROUTER_API_KEY, "  ")
        .await
        .unwrap();
    let err = client
        .chat_for(AiTask::Metadata, "", "q", 10)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("openrouter_api_key is not set"),
        "{err:#}"
    );
    assert!(openrouter.received_requests().await.unwrap().is_empty());
}

/// A database that cannot be read sends the default model.
#[tokio::test]
async fn an_unreadable_model_is_the_default() {
    let pool = pool().await;
    set_setting(&pool, "ai_model_spotify", "x/y").await.unwrap();
    assert_eq!(model_of(&pool, AiTask::Spotify).await, "x/y");
    pool.close().await;
    assert_eq!(
        model_of(&pool, AiTask::Spotify).await,
        DEFAULT_OPENROUTER_MODEL
    );
}

/// `chat_json_for` strips the fences and parses the answer.
#[tokio::test]
async fn chat_json_for_parses_the_answer() {
    let pool = pool().await;
    let openrouter = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(answer("```json\n{\"song\":\"S\",\"artist\":\"A\"}\n```")),
        )
        .mount(&openrouter)
        .await;
    let client = AiClient::with_openrouter(
        pool.clone(),
        OpenRouterClient::new(format!("{}/api/v1", openrouter.uri())),
    );
    set_setting(&pool, SETTING_OPENROUTER_API_KEY, KEY)
        .await
        .unwrap();
    #[derive(serde::Deserialize)]
    struct Title {
        song: String,
        artist: String,
    }
    let t: Title = client
        .chat_json_for(AiTask::Metadata, "sys", "title")
        .await
        .unwrap();
    assert_eq!((t.song.as_str(), t.artist.as_str()), ("S", "A"));
}
