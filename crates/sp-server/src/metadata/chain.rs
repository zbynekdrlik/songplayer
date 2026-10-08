//! The ONE metadata provider chain (#136).
//!
//! `lib.rs` builds it once with [`provider_chain`] and hands the same `Arc`
//! to the download worker, the reprocess worker and the API (the probe route
//! and `status.metadata`). Before #136 the download worker had [Claude,
//! Gemini] and the reprocess worker Gemini ALONE — two lists that diverged,
//! so with Gemini broken the only repair path had no working provider and
//! ~110 rows kept their raw YouTube title while Claude answered correctly.
//!
//! Every provider is wrapped in [`Recorded`]: each call's outcome and latency
//! lands in the chain's [`MetadataHealth`] and in the log, whoever calls it.
//!
//! #229 item C: the production chain is gated on the node's paid-AI switch
//! ([`ProviderChain::providers`] answers none while it is off, read at every
//! walk): every metadata path asks it, so no provider is called then.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use sp_core::metadata::VideoMetadata;

use super::claude::ClaudeMetadataProvider;
use super::gemini::GeminiProvider;
use super::health::{MetadataHealth, ProviderHealth};
use super::{MetadataError, MetadataProvider};
use crate::ai::client::AiClient;

/// The ordered providers every metadata path uses, each recording its calls.
pub struct ProviderChain {
    providers: Vec<Box<dyn MetadataProvider>>,
    health: Arc<MetadataHealth>,
    /// #229 item C: the database whose paid-AI switch gates every walk
    /// ([`ProviderChain::gated`]); `None` = never gated (a test's chain).
    paid_ai: Option<sqlx::SqlitePool>,
}

impl ProviderChain {
    /// A chain trying `providers` in this order; every call is recorded.
    pub fn new(providers: Vec<Box<dyn MetadataProvider>>) -> Self {
        let health = Arc::new(MetadataHealth::new(
            providers.iter().map(|p| p.name().to_string()),
        ));
        let providers = providers
            .into_iter()
            .enumerate()
            .map(|(index, inner)| {
                Box::new(Recorded {
                    inner,
                    index,
                    health: Arc::clone(&health),
                }) as Box<dyn MetadataProvider>
            })
            .collect();
        Self {
            providers,
            health,
            paid_ai: None,
        }
    }

    /// #229 item C: this chain asks the paid-AI switch of `pool` before
    /// every walk (`paid_ai::enabled`, read live).
    pub fn gated(mut self, pool: sqlx::SqlitePool) -> Self {
        self.paid_ai = Some(pool);
        self
    }

    /// The providers in chain order, or `None` while this node's paid AI is
    /// off (#229 item C): then no provider may be asked.
    pub async fn providers(&self) -> Option<&[Box<dyn MetadataProvider>]> {
        if let Some(pool) = &self.paid_ai
            && !crate::paid_ai::enabled(pool).await
        {
            return None;
        }
        Some(&self.providers)
    }

    /// Every provider's health, in chain order.
    pub fn health(&self) -> Vec<ProviderHealth> {
        self.health.snapshot()
    }
}

/// The production chain: Claude through CLIProxyAPI first, then Gemini on the
/// `gemini_api_key` key LIST (split here with the shared
/// `gemini_keys_from_setting`, one key per attempt). Gemini is in the chain
/// even with no key: it then fails at once with "no API key configured", so
/// `status.metadata` names the misconfiguration instead of hiding a provider.
/// Gated on `pool`'s paid-AI switch (#229 item C, [`ProviderChain::gated`]).
pub fn provider_chain(
    pool: &sqlx::SqlitePool,
    ai_client: Arc<AiClient>,
    gemini_csv: &str,
    gemini_model: &str,
) -> Arc<ProviderChain> {
    let api_root = crate::gemini_api::GEMINI_API_ROOT;
    let chain = chain_at(ai_client, gemini_csv, gemini_model, api_root);
    Arc::new(chain.gated(pool.clone()))
}

/// [`provider_chain`] with Gemini's API root injected (a mock server in
/// tests), not gated.
pub fn provider_chain_at(
    ai_client: Arc<AiClient>,
    gemini_csv: &str,
    gemini_model: &str,
    gemini_api_root: &str,
) -> Arc<ProviderChain> {
    Arc::new(chain_at(
        ai_client,
        gemini_csv,
        gemini_model,
        gemini_api_root,
    ))
}

/// Claude, then Gemini on `gemini_api_root`.
fn chain_at(
    ai_client: Arc<AiClient>,
    gemini_csv: &str,
    gemini_model: &str,
    gemini_api_root: &str,
) -> ProviderChain {
    let keys = crate::gemini_api::gemini_keys_from_setting(gemini_csv);
    // The key COUNT only — never a key.
    tracing::info!(
        gemini_keys = keys.len(),
        gemini_model,
        "metadata provider chain: claude, then gemini"
    );
    ProviderChain::new(vec![
        Box::new(ClaudeMetadataProvider::new(ai_client)),
        Box::new(GeminiProvider::with_api_root(
            keys,
            gemini_model.to_string(),
            gemini_api_root,
        )),
    ])
}

/// A chain member: the provider plus its slot in the chain's health record.
struct Recorded {
    inner: Box<dyn MetadataProvider>,
    index: usize,
    health: Arc<MetadataHealth>,
}

#[async_trait]
impl MetadataProvider for Recorded {
    async fn extract(&self, video_id: &str, title: &str) -> Result<VideoMetadata, MetadataError> {
        let started = Instant::now();
        let outcome = self.inner.extract(video_id, title).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let provider = self.inner.name();
        match &outcome {
            Ok(meta) => {
                self.health
                    .record_ok(self.index, chrono::Utc::now().timestamp_millis());
                tracing::info!(
                    provider, video_id, elapsed_ms, song = %meta.song, artist = %meta.artist,
                    "metadata provider answered"
                );
            }
            Err(e) => {
                self.health.record_error(self.index, &e.to_string());
                let error = super::health::bounded_error(&e.to_string());
                tracing::warn!(provider, video_id, elapsed_ms, %error, "metadata provider failed");
            }
        }
        outcome
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

#[cfg(test)]
#[path = "chain_tests.rs"]
mod tests;
