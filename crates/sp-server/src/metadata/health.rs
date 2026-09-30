//! Metadata provider health (#136) — what `GET /api/v1/status.metadata`
//! reports: how many videos still carry title-parser metadata because every
//! provider failed, and, per provider of the production chain, when it last
//! answered and why its last call failed.
//!
//! Before #136 a whole provider could die (a malformed key, a retired model,
//! an unauthenticated proxy) while every new video quietly fell back to the
//! title parser: nothing on the status said so. The chain records every
//! provider call here (`chain::Recorded`), so the download worker, the
//! reprocess worker and the probe route all feed the same record.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

/// The reprocess worker's queue: rows whose metadata came from the title
/// parser because every provider failed. The worker selects exactly these
/// rows and `failed_videos` counts exactly these, so the count drains to 0 as
/// the worker repairs them. A row the operator corrected on the dashboard
/// (`metadata_source = 'manual'`, set by `PATCH /api/v1/videos/{id}`) is
/// never in it: the repair would write over the correction (#136).
pub const REPAIR_QUEUE_WHERE: &str =
    "gemini_failed = 1 AND normalized = 1 AND metadata_source IS NOT 'manual'";

/// Characters of a provider error kept on the status / in a probe answer: a
/// Claude error carries the proxy's whole reply, an LLM answer can be long.
pub const MAX_ERROR_CHARS: usize = 300;

/// `error` cut to [`MAX_ERROR_CHARS`] characters.
pub fn bounded_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

/// One provider's health, as `status.metadata.providers[]` shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub name: String,
    /// Unix ms of the provider's last answer; `None` = none since start.
    pub last_ok_at_ms: Option<i64>,
    /// The error of the provider's LAST call; `None` once it answers again.
    pub last_error: Option<String>,
}

/// `GET /api/v1/status.metadata`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataStatus {
    /// Rows in the repair queue ([`REPAIR_QUEUE_WHERE`]); `None` when the
    /// count could not be read (logged) — never a false 0.
    pub failed_videos: Option<i64>,
    /// The chain's providers, in chain order.
    pub providers: Vec<ProviderHealth>,
}

/// The per-provider records of one chain, in chain order.
pub struct MetadataHealth {
    providers: Mutex<Vec<ProviderHealth>>,
}

impl MetadataHealth {
    /// One empty record per provider name, in chain order.
    pub fn new(names: impl IntoIterator<Item = String>) -> Self {
        let providers = names
            .into_iter()
            .map(|name| ProviderHealth {
                name,
                ..ProviderHealth::default()
            })
            .collect();
        Self {
            providers: Mutex::new(providers),
        }
    }

    /// Provider `index` answered at `at_ms`: its last error is cleared.
    pub fn record_ok(&self, index: usize, at_ms: i64) {
        let mut providers = self.lock();
        if let Some(p) = providers.get_mut(index) {
            p.last_ok_at_ms = Some(at_ms);
            p.last_error = None;
        }
    }

    /// Provider `index` failed with `error`, kept to [`MAX_ERROR_CHARS`] (a
    /// provider error text never carries a key — `gemini::GeminiProvider`
    /// redacts them).
    pub fn record_error(&self, index: usize, error: &str) {
        let mut providers = self.lock();
        if let Some(p) = providers.get_mut(index) {
            p.last_error = Some(bounded_error(error));
        }
    }

    /// Every provider's record, in chain order.
    pub fn snapshot(&self) -> Vec<ProviderHealth> {
        self.lock().clone()
    }

    /// The records survive a panic in another holder (a poisoned lock still
    /// holds consistent data: every write is a plain field store).
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<ProviderHealth>> {
        self.providers.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The number of rows in the repair queue ([`REPAIR_QUEUE_WHERE`]).
pub async fn failed_videos(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    let sql = format!("SELECT COUNT(*) AS c FROM videos WHERE {REPAIR_QUEUE_WHERE}");
    let row = sqlx::query(&sql).fetch_one(pool).await?;
    Ok(row.get("c"))
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
