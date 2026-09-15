//! CLIProxyAPI child-process watchdog — extracted from `lib.rs` to keep it
//! under the 1000-line cap. Polls the proxy and restarts it if it died.

use std::sync::Arc;

use tracing::{info, warn};

use crate::ai;

/// Interval between ai_proxy health checks.
const AI_PROXY_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Poll the CLIProxyAPI child every `AI_PROXY_WATCHDOG_INTERVAL_SECS` and
/// restart it if it died. Without this, a proxy crash mid-processing
/// leaves the worker silently falling through to "no text sources
/// available" for every subsequent song (2026-04-19 event). Exits on
/// shutdown broadcast.
pub(crate) async fn run(
    proxy: Arc<ai::proxy::ProxyManager>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let interval = std::time::Duration::from_secs(AI_PROXY_WATCHDOG_INTERVAL_SECS);
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(interval) => {
                let status = proxy.status().await;
                if status.running {
                    continue;
                }
                warn!("ai_proxy watchdog: proxy is down, attempting restart");
                match proxy.start().await {
                    Ok(()) => info!("ai_proxy watchdog: restart succeeded"),
                    Err(e) => warn!("ai_proxy watchdog: restart failed: {e}"),
                }
            }
        }
    }
}
