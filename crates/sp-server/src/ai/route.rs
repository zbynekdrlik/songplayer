//! #321: each text AI task's call, read live per call.
//!
//! A child module of `client.rs` (it reaches `AiClient`'s private fields).
//! Every `chat_for(task, …)` reads the task's model from the database
//! (`ai_model_<task>`, blank = [`sp_core::ai_tasks::DEFAULT_OPENROUTER_MODEL`])
//! and sends it to OpenRouter ([`AiClient::openrouter_chat`]). An old
//! `ai_provider_<task>` row (the deleted `claude_proxy` / `openrouter`
//! choice) is no longer read: every task goes to OpenRouter. V45 deletes such
//! rows and a settings PATCH refuses them
//! (`sp_core::ai_tasks::is_retired_setting`).
//!
//! The call asks `paid_ai::enabled` itself before anything is sent (the call
//! sites ask it too; this is the transport's own check, so a new call site
//! can never bypass it) and reads `openrouter_api_key` live, so a key, model
//! or switch change needs no restart.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use sp_core::ai_tasks::{AiTask, SETTING_OPENROUTER_API_KEY, openrouter_model};
use sqlx::SqlitePool;
use tracing::warn;

use super::AiClient;
use super::strip_markdown_fences;
use crate::ai::openrouter::{Ask, Completion};

/// The most a task's completion may write.
pub const MAX_TOKENS: u32 = 32_000;

/// The OpenRouter model of `task`, read from the database now. A failed read
/// WARNs and sends the default model.
pub async fn model_of(pool: &SqlitePool, task: AiTask) -> String {
    match crate::db::models::get_setting(pool, &task.model_setting()).await {
        Ok(model) => openrouter_model(model.as_deref()),
        Err(e) => {
            let default = openrouter_model(None);
            warn!(task = task.as_str(), %e, model = %default, "ai: reading the task's model failed - the default is sent");
            default
        }
    }
}

impl AiClient {
    /// The model `task` sends now.
    pub async fn model(&self, task: AiTask) -> String {
        model_of(&self.pool, task).await
    }

    /// `task`'s chat completion with its model read now; the raw text.
    pub async fn chat_for(
        &self,
        task: AiTask,
        system: &str,
        user: &str,
        timeout_secs: u64,
    ) -> Result<String> {
        let model = self.model(task).await;
        self.chat_with(task, &model, system, user, timeout_secs)
            .await
    }

    /// `task`'s chat completion with a model the caller read once
    /// ([`Self::model`]), so a log line after the call names the model the
    /// call used even when the setting changed meanwhile.
    pub async fn chat_with(
        &self,
        task: AiTask,
        model: &str,
        system: &str,
        user: &str,
        timeout_secs: u64,
    ) -> Result<String> {
        self.openrouter_chat(model, system, user, timeout_secs, MAX_TOKENS)
            .await
            .map(|c| c.text)
            .with_context(|| format!("{} via OpenRouter ({model})", task.as_str()))
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
    /// paid AI is off or with no key.
    pub async fn openrouter_chat(
        &self,
        model: &str,
        system: &str,
        user: &str,
        timeout_secs: u64,
        max_tokens: u32,
    ) -> Result<Completion> {
        if !crate::paid_ai::enabled(&self.pool).await {
            anyhow::bail!(crate::paid_ai::OFF_REASON);
        }
        let key = crate::db::models::get_setting(&self.pool, SETTING_OPENROUTER_API_KEY)
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
        self.openrouter.complete(&ask).await
    }
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
