//! #321: where each text AI task's call goes, read live per call.
//!
//! A child module of `client.rs` (it reaches `AiClient`'s private fields).
//! `AiClient::with_routing(pool)` (production, `lib.rs`) makes every
//! `chat_for(task, …)` read the task's two settings from the database
//! (`sp_core::ai_tasks`): `ai_provider_<task>` unset / blank /
//! `claude_proxy` → exactly the proxy call it was before #321
//! (`chat_with_timeout`); `openrouter` → [`AiClient::openrouter_chat`] with
//! `ai_model_<task>` (blank = the newest Gemini). A client with no routing
//! (`AiClient::new`, every unit test of a call site) stays on the proxy.
//!
//! The OpenRouter path asks `paid_ai::enabled` itself before anything is
//! sent (the call sites ask it too; this is the transport's own check, so a
//! new call site can never bypass it) and reads `openrouter_api_key` live,
//! so a key or switch change needs no restart.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use sp_core::ai_tasks::{AiProvider, AiTask, SETTING_OPENROUTER_API_KEY, openrouter_model};
use sqlx::SqlitePool;
use tracing::warn;

use super::AiClient;
use super::strip_markdown_fences;
use crate::ai::openrouter::{Ask, Completion, OpenRouterClient};

/// The most a completion may write (the proxy client's bound).
pub const MAX_TOKENS: u32 = 32_000;

/// The routing an `AiClient` reads its tasks' settings with.
pub struct Routing {
    pub(super) pool: SqlitePool,
    pub(super) openrouter: OpenRouterClient,
}

/// Where one task's call goes now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Claude through CLIProxyAPI (the client's own `ai_model`).
    Proxy,
    /// OpenRouter with this model.
    OpenRouter { model: String },
}

/// The route of a task from its two stored values (pure).
pub fn plan(provider: Option<&str>, model: Option<&str>) -> Route {
    match AiProvider::read(provider) {
        AiProvider::ClaudeProxy => Route::Proxy,
        AiProvider::OpenRouter => Route::OpenRouter {
            model: openrouter_model(model),
        },
    }
}

/// The route of `task`, read from the database now. A failed read WARNs
/// and stays on the proxy: the behaviour before #321.
pub async fn route_of(pool: &SqlitePool, task: AiTask) -> Route {
    let read = |key: String| async move { crate::db::models::get_setting(pool, &key).await };
    match (
        read(task.provider_setting()).await,
        read(task.model_setting()).await,
    ) {
        (Ok(provider), Ok(model)) => plan(provider.as_deref(), model.as_deref()),
        (Err(e), _) | (_, Err(e)) => {
            warn!(task = task.as_str(), %e, "ai route: reading the task's settings failed - the proxy is asked");
            Route::Proxy
        }
    }
}

impl AiClient {
    /// The same client, routing each task by its settings in `pool`, with
    /// OpenRouter at its public API root.
    pub fn with_routing(self, pool: SqlitePool) -> Self {
        self.with_routing_to(
            pool,
            OpenRouterClient::new(sp_core::ai_tasks::OPENROUTER_API_URL),
        )
    }

    /// The same client with routing through `openrouter` (tests: a mock).
    pub fn with_routing_to(mut self, pool: SqlitePool, openrouter: OpenRouterClient) -> Self {
        self.routing = Some(Routing { pool, openrouter });
        self
    }

    /// Where `task` goes now: the proxy when this client has no routing.
    pub async fn route(&self, task: AiTask) -> Route {
        match &self.routing {
            Some(r) => route_of(&r.pool, task).await,
            None => Route::Proxy,
        }
    }

    /// The provider and model `task` goes to now, for a log line.
    pub async fn route_label(&self, task: AiTask) -> String {
        match self.route(task).await {
            Route::Proxy => format!("claude_proxy:{}", self.settings.model),
            Route::OpenRouter { model } => format!("openrouter:{model}"),
        }
    }

    /// `task`'s chat completion, sent where its settings say now (the
    /// module doc); the raw text.
    pub async fn chat_for(
        &self,
        task: AiTask,
        system: &str,
        user: &str,
        timeout_secs: u64,
    ) -> Result<String> {
        match self.route(task).await {
            Route::Proxy => self.chat_with_timeout(system, user, timeout_secs).await,
            Route::OpenRouter { model } => self
                .openrouter_chat(&model, system, user, timeout_secs, MAX_TOKENS)
                .await
                .map(|c| c.text)
                .with_context(|| format!("{} via OpenRouter ({model})", task.as_str())),
        }
    }

    /// [`Self::chat_for`] parsed as JSON (markdown fences stripped).
    pub async fn chat_json_for<T: DeserializeOwned>(
        &self,
        task: AiTask,
        system: &str,
        user: &str,
    ) -> Result<T> {
        let raw = self.chat_for(task, system, user, 300).await?;
        let cleaned = strip_markdown_fences(&raw);
        serde_json::from_str(&cleaned)
            .with_context(|| format!("failed to parse LLM response as JSON: {cleaned}"))
    }

    /// One OpenRouter completion with `model`: refused (nothing sent) while
    /// paid AI is off, with no routing, or with no key.
    pub async fn openrouter_chat(
        &self,
        model: &str,
        system: &str,
        user: &str,
        timeout_secs: u64,
        max_tokens: u32,
    ) -> Result<Completion> {
        let Some(routing) = &self.routing else {
            anyhow::bail!("this AI client has no OpenRouter routing");
        };
        if !crate::paid_ai::enabled(&routing.pool).await {
            anyhow::bail!(crate::paid_ai::OFF_REASON);
        }
        let key = crate::db::models::get_setting(&routing.pool, SETTING_OPENROUTER_API_KEY)
            .await
            .context("reading openrouter_api_key")?
            .unwrap_or_default();
        let key = key.trim();
        if key.is_empty() {
            anyhow::bail!("{SETTING_OPENROUTER_API_KEY} is not set: nothing was sent");
        }
        let ask = Ask {
            key,
            model,
            system,
            user,
            max_tokens,
            timeout: Duration::from_secs(timeout_secs),
        };
        routing.openrouter.complete(&ask).await
    }
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
