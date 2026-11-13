//! #321: each task's route, read live; the proxy unchanged by default.

use sp_core::ai_tasks::{AiTask, DEFAULT_OPENROUTER_MODEL, SETTING_OPENROUTER_API_KEY};
use sp_core::config::SETTING_PAID_AI_ENABLED;
use sqlx::SqlitePool;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{Route, plan, route_of};
use crate::ai::AiSettings;
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

/// A proxy mock and an OpenRouter mock, each answering its own word, and a
/// routed client on both.
async fn rig(pool: &SqlitePool) -> (MockServer, MockServer, AiClient) {
    let proxy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({"model": "claude-test"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer("from the proxy")))
        .mount(&proxy)
        .await;
    let openrouter = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer("from openrouter")))
        .mount(&openrouter)
        .await;
    let client = AiClient::new(AiSettings {
        api_url: format!("{}/v1", proxy.uri()),
        api_key: None,
        model: "claude-test".into(),
        system_prompt_extra: None,
    })
    .with_retry_policy(RetryPolicy::NO_WAIT)
    .with_routing_to(
        pool.clone(),
        OpenRouterClient::new(format!("{}/api/v1", openrouter.uri()))
            .with_retry_policy(RetryPolicy::NO_WAIT),
    );
    (proxy, openrouter, client)
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

#[test]
fn the_plan_of_a_task() {
    assert_eq!(plan(None, Some("x/y")), Route::Proxy);
    assert_eq!(plan(Some("claude_proxy"), None), Route::Proxy);
    assert_eq!(plan(Some("bogus"), None), Route::Proxy);
    assert_eq!(
        plan(Some("openrouter"), None),
        Route::OpenRouter {
            model: DEFAULT_OPENROUTER_MODEL.into()
        }
    );
    assert_eq!(
        plan(Some("openrouter"), Some(" google/gemini-4-carbon ")),
        Route::OpenRouter {
            model: "google/gemini-4-carbon".into()
        }
    );
}

/// Nothing set: every task goes to the proxy, exactly as before #321.
#[tokio::test]
async fn with_nothing_set_every_task_goes_to_the_proxy() {
    let pool = pool().await;
    let (proxy, openrouter, client) = rig(&pool).await;
    set_setting(&pool, SETTING_OPENROUTER_API_KEY, KEY)
        .await
        .unwrap();
    for task in AiTask::ALL {
        assert_eq!(
            client.chat_for(task, "", "q", 10).await.unwrap(),
            "from the proxy"
        );
        assert_eq!(client.route_label(task).await, "claude_proxy:claude-test");
    }
    assert_eq!(proxy.received_requests().await.unwrap().len(), 6);
    assert!(openrouter.received_requests().await.unwrap().is_empty());
}

/// One task switched: only that task goes to OpenRouter, with its model,
/// read live (no restart); switched back, it is on the proxy again.
#[tokio::test]
async fn a_switched_task_goes_to_openrouter_with_its_model_read_live() {
    let pool = pool().await;
    let (proxy, openrouter, client) = rig(&pool).await;
    set_setting(&pool, SETTING_OPENROUTER_API_KEY, KEY)
        .await
        .unwrap();
    set_setting(&pool, "ai_provider_translate", "openrouter")
        .await
        .unwrap();
    set_setting(&pool, "ai_model_translate", "google/gemini-4-carbon")
        .await
        .unwrap();
    assert_eq!(
        client
            .chat_for(AiTask::Translate, "", "q", 10)
            .await
            .unwrap(),
        "from openrouter"
    );
    assert_eq!(
        client.route_label(AiTask::Translate).await,
        "openrouter:google/gemini-4-carbon"
    );
    assert_eq!(
        client
            .chat_for(AiTask::Register, "", "q", 10)
            .await
            .unwrap(),
        "from the proxy"
    );
    set_setting(&pool, "ai_model_translate", "").await.unwrap();
    client
        .chat_for(AiTask::Translate, "", "q", 10)
        .await
        .unwrap();
    assert_eq!(
        model_sent(&openrouter.received_requests().await.unwrap()),
        vec!["google/gemini-4-carbon", DEFAULT_OPENROUTER_MODEL]
    );
    set_setting(&pool, "ai_provider_translate", "claude_proxy")
        .await
        .unwrap();
    assert_eq!(
        client
            .chat_for(AiTask::Translate, "", "q", 10)
            .await
            .unwrap(),
        "from the proxy"
    );
    assert_eq!(proxy.received_requests().await.unwrap().len(), 2);
}

/// Paid AI off: the OpenRouter path sends nothing and says why.
#[tokio::test]
async fn openrouter_sends_nothing_while_paid_ai_is_off() {
    let pool = pool().await;
    let (_proxy, openrouter, client) = rig(&pool).await;
    set_setting(&pool, SETTING_OPENROUTER_API_KEY, KEY)
        .await
        .unwrap();
    set_setting(&pool, "ai_provider_cleanup", "openrouter")
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
async fn openrouter_with_no_key_sends_nothing() {
    let pool = pool().await;
    let (_proxy, openrouter, client) = rig(&pool).await;
    set_setting(&pool, "ai_provider_metadata", "openrouter")
        .await
        .unwrap();
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

/// A client with no routing (every call site's unit test) never reads a
/// route and refuses an OpenRouter call.
#[tokio::test]
async fn a_client_with_no_routing_stays_on_the_proxy() {
    let client = AiClient::new(AiSettings::default());
    assert_eq!(client.route(AiTask::Translate).await, Route::Proxy);
    let err = client
        .openrouter_chat("m", "", "q", 5, 16)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no OpenRouter routing"), "{err}");
}

/// A database that cannot be read keeps the task on the proxy.
#[tokio::test]
async fn an_unreadable_route_is_the_proxy() {
    let pool = pool().await;
    set_setting(&pool, "ai_provider_spotify", "openrouter")
        .await
        .unwrap();
    assert!(matches!(
        route_of(&pool, AiTask::Spotify).await,
        Route::OpenRouter { .. }
    ));
    pool.close().await;
    assert_eq!(route_of(&pool, AiTask::Spotify).await, Route::Proxy);
}

/// `chat_json_for` strips the fences and parses the routed answer.
#[tokio::test]
async fn chat_json_for_parses_the_routed_answer() {
    let pool = pool().await;
    let openrouter = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(answer("```json\n{\"song\":\"S\",\"artist\":\"A\"}\n```")),
        )
        .mount(&openrouter)
        .await;
    let client = AiClient::new(AiSettings::default()).with_routing_to(
        pool.clone(),
        OpenRouterClient::new(format!("{}/api/v1", openrouter.uri())),
    );
    set_setting(&pool, SETTING_OPENROUTER_API_KEY, KEY)
        .await
        .unwrap();
    set_setting(&pool, "ai_provider_metadata", "openrouter")
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
