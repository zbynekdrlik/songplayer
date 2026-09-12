//! dantesync clock-health polling + evaluation (#146, contract §D/§7).
//!
//! A 1 Hz background task polls the local dantesync status endpoint and
//! reduces it to a [`ClockHealth`] that the NDI health snapshot carries. The
//! reduction is pure ([`evaluate`]) so it is unit-tested against the real
//! payload sample; the poller is thin I/O glue.
//!
//! `clock_ok = is_locked && mode ∈ {LOCK, NANO}`. A missing/unreachable
//! endpoint yields `clock_ok = false` with reason `"no dantesync"` and MUST
//! never block playback (contract §4).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Default dantesync status endpoint (overridable via `DANTESYNC_STATUS_URL`).
pub const DANTESYNC_STATUS_URL_DEFAULT: &str = "http://127.0.0.1:8898/status";

/// Reduced clock health exposed on the NDI health snapshot + dashboard.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClockHealth {
    pub is_locked: bool,
    pub mode: String,
    pub offset_ns: Option<i64>,
    pub ntp_failed: Option<bool>,
    pub ntp_age_s: Option<i64>,
    /// `is_locked && mode ∈ {LOCK, NANO}` — the CLOCK-OK precondition.
    pub clock_ok: bool,
    /// Human reason when `!clock_ok` (e.g. `"no dantesync"`).
    pub reason: Option<String>,
    /// RFC-3339 timestamp of the poll that produced this value; `None` for the
    /// default/never-polled state.
    pub sampled_at: Option<String>,
}

impl Default for ClockHealth {
    fn default() -> Self {
        evaluate(None)
    }
}

/// Raw dantesync `/status` payload. Every field is optional so a partial or
/// older payload still deserialises; unknown fields are ignored by serde.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct DantesyncStatus {
    #[serde(default)]
    pub is_locked: Option<bool>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub offset_ns: Option<i64>,
    #[serde(default)]
    pub ntp_failed: Option<bool>,
    #[serde(default)]
    pub ntp_age_s: Option<i64>,
}

/// Pure reduction of a dantesync payload to [`ClockHealth`]. `None` (endpoint
/// missing/unreachable/unparseable) → not ok, reason `"no dantesync"`.
pub fn evaluate(payload: Option<&DantesyncStatus>) -> ClockHealth {
    match payload {
        None => ClockHealth {
            is_locked: false,
            mode: String::new(),
            offset_ns: None,
            ntp_failed: None,
            ntp_age_s: None,
            clock_ok: false,
            reason: Some("no dantesync".to_string()),
            sampled_at: None,
        },
        Some(s) => {
            let is_locked = s.is_locked.unwrap_or(false);
            let mode = s.mode.clone().unwrap_or_default();
            let clock_ok = is_locked && (mode == "LOCK" || mode == "NANO");
            let reason = if clock_ok {
                None
            } else {
                Some(format!(
                    "clock not ok (is_locked={is_locked}, mode={mode:?})"
                ))
            };
            ClockHealth {
                is_locked,
                mode,
                offset_ns: s.offset_ns,
                ntp_failed: s.ntp_failed,
                ntp_age_s: s.ntp_age_s,
                clock_ok,
                reason,
                sampled_at: None,
            }
        }
    }
}

/// Poll the dantesync endpoint once (500 ms timeout). Never returns an error —
/// any failure maps to the `"no dantesync"` health so playback never blocks.
///
/// mutants::skip — network I/O only exercised against a live dantesync
/// endpoint; the reduction it wraps ([`evaluate`]) is unit-tested.
#[cfg_attr(test, mutants::skip)]
async fn poll_once(client: &reqwest::Client, url: &str) -> ClockHealth {
    let sampled_at = Some(chrono::Utc::now().to_rfc3339());
    let mut health = match client
        .get(url)
        .timeout(Duration::from_millis(500))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<DantesyncStatus>().await {
            Ok(status) => evaluate(Some(&status)),
            Err(_) => evaluate(None),
        },
        Err(_) => evaluate(None),
    };
    health.sampled_at = sampled_at;
    health
}

/// Spawn the 1 Hz dantesync poller. Writes the latest [`ClockHealth`] into
/// `shared` and WARN-logs only on `clock_ok` state changes (never every tick).
/// Exits on the shutdown broadcast.
///
/// mutants::skip — background I/O task; behaviour verified live via
/// `GET /api/v1/ndi/health`.
#[cfg_attr(test, mutants::skip)]
pub fn spawn_clock_health_poller(
    client: reqwest::Client,
    url: String,
    shared: Arc<RwLock<ClockHealth>>,
    mut shutdown: broadcast::Receiver<()>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_ok: Option<bool> = None;
        info!(%url, "clock-health: dantesync poller started");
        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    info!("clock-health: poller shutting down");
                    break;
                }
                _ = ticker.tick() => {
                    let health = poll_once(&client, &url).await;
                    if last_ok != Some(health.clock_ok) {
                        warn!(
                            clock_ok = health.clock_ok,
                            mode = %health.mode,
                            reason = health.reason.as_deref().unwrap_or(""),
                            "clock-health: state changed"
                        );
                        last_ok = Some(health.clock_ok);
                    }
                    if let Ok(mut w) = shared.write() {
                        *w = health;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
#[path = "clock_health_tests.rs"]
mod clock_health_tests;
