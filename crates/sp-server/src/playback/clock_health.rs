//! dantesync clock-health polling (#146, contract §D/§7).
//!
//! A 1 Hz background task polls the local dantesync status endpoint and
//! reduces it — via [`evaluate`], re-exported from `sp_core::clock_health` —
//! to a [`ClockHealth`] the NDI health snapshot carries. The vocabulary
//! ([`ClockHealth`], [`DantesyncStatus`], [`evaluate`]) lives in `sp-core` so
//! #150's WASM badge shares it and is unit-tested there; this module is the
//! thin reqwest/tokio I/O glue.
//!
//! `clock_ok = is_locked && mode ∈ {LOCK, NANO}`. A missing/unreachable
//! endpoint yields `clock_ok = false` with reason `"no dantesync"` and MUST
//! never block playback (contract §4).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::broadcast;
use tracing::{info, warn};

pub use sp_core::clock_health::{ClockHealth, DantesyncStatus, evaluate};

/// Default dantesync status endpoint (overridable via `DANTESYNC_STATUS_URL`).
pub const DANTESYNC_STATUS_URL_DEFAULT: &str = "http://127.0.0.1:8898/status";

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
/// `shared` and logs only on `clock_ok` state changes (INFO on recovery into
/// ok, WARN on loss; never every tick). Exits on the shutdown broadcast.
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
                        // Transition INTO clock_ok=true is normal recovery
                        // (info); INTO clock_ok=false is a loss worth a warn.
                        if health.clock_ok {
                            info!(
                                clock_ok = health.clock_ok,
                                mode = %health.mode,
                                reason = health.reason.as_deref().unwrap_or(""),
                                "clock-health: state changed"
                            );
                        } else {
                            warn!(
                                clock_ok = health.clock_ok,
                                mode = %health.mode,
                                reason = health.reason.as_deref().unwrap_or(""),
                                "clock-health: state changed"
                            );
                        }
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
