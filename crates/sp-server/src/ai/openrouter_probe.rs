//! #321: the live gate of the OpenRouter text path.
//!
//! `POST /api/v1/ai/openrouter/probe {"model"?}` sends ONE tiny completion
//! through the very call every task's OpenRouter route makes
//! (`AiClient::openrouter_chat`: the paid-AI switch, the key read live, the
//! client and its retries) and answers what happened. The post-deploy spec
//! `e2e/post-deploy-openrouter.spec.ts` calls it on SNV with the default
//! model (the owner's rule: every external boundary gets a live gate).
//! Paid AI off or no key: `ok: false` with the reason, nothing sent.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::ai::client::AiClient;

/// The probe's prompt: one word back.
pub const PROBE_PROMPT: &str = "Reply with the single word: ok";

/// The probe's output bound: room for a reasoning model's thinking before
/// its one word (a tiny bound can leave such a model with no text).
pub const PROBE_MAX_TOKENS: u32 = 1_000;

/// The probe's bound per request (s); the spec waits longer.
pub const PROBE_TIMEOUT_S: u64 = 60;

/// How many characters of the answer the report keeps.
pub const SAMPLE_CHARS: usize = 80;

/// The route's body: the model to probe; none or blank = the default.
#[derive(Debug, Default, Deserialize)]
pub struct ProbeRequest {
    #[serde(default)]
    pub model: Option<String>,
}

/// What the probe answers (always with 200).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeReport {
    pub ok: bool,
    /// The model asked for.
    pub model: String,
    /// The model OpenRouter says answered (it may name a dated version).
    pub answered_model: Option<String>,
    pub latency_ms: u64,
    /// Why it failed (never the key); `None` when ok.
    pub error: Option<String>,
    /// The answer's first characters.
    pub sample: String,
}

/// The report of one probe's outcome (pure).
pub fn report(
    model: String,
    outcome: anyhow::Result<crate::ai::openrouter::Completion>,
    latency_ms: u64,
) -> ProbeReport {
    match outcome {
        Ok(c) => ProbeReport {
            ok: true,
            model,
            answered_model: c.model,
            latency_ms,
            error: None,
            sample: c.text.trim().chars().take(SAMPLE_CHARS).collect(),
        },
        Err(e) => ProbeReport {
            ok: false,
            model,
            answered_model: None,
            latency_ms,
            error: Some(format!("{e:#}")),
            sample: String::new(),
        },
    }
}

/// Probe `model` (blank = the default) through `client`.
pub async fn run_probe(client: &AiClient, model: Option<&str>) -> ProbeReport {
    let model = sp_core::ai_tasks::openrouter_model(model);
    let started = Instant::now();
    let outcome = client
        .openrouter_chat(&model, "", PROBE_PROMPT, PROBE_TIMEOUT_S, PROBE_MAX_TOKENS)
        .await;
    report(model, outcome, started.elapsed().as_millis() as u64)
}

#[cfg(test)]
#[path = "openrouter_probe_tests.rs"]
mod tests;
