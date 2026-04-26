//! Dashboard alert for Resolume push-chain health.
//!
//! Quiet by default — renders nothing when every host is healthy.
//! Surfaces a compact alert only when a real problem exists:
//! - circuit breaker open, OR
//! - consecutive refresh failures, OR
//! - any expected SongPlayer token (`#sp-title`, `#sp-subs`,
//!   `#sp-subs-next`, `#sp-subssk`) has zero clips in the composition.
//!
//! Polls /api/v1/resolume/health every 5 s.

use std::collections::BTreeMap;

use leptos::prelude::*;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct HostHealth {
    pub host: String,
    pub last_refresh_ts: Option<String>,
    pub last_refresh_ok: bool,
    pub consecutive_failures: u32,
    pub circuit_breaker_open: bool,
    pub clips_by_token: BTreeMap<String, usize>,
}

impl HostHealth {
    /// Short human reason this host is unhealthy, or `None` if healthy.
    fn problem(&self) -> Option<String> {
        if self.circuit_breaker_open {
            return Some("circuit open — Resolume unreachable".into());
        }
        if self.consecutive_failures > 0 {
            return Some(format!(
                "refresh failing ({} consecutive)",
                self.consecutive_failures
            ));
        }
        let missing: Vec<&str> = self
            .clips_by_token
            .iter()
            .filter(|(_, &n)| n == 0)
            .map(|(k, _)| k.as_str())
            .collect();
        if !missing.is_empty() {
            return Some(format!("missing clips: {}", missing.join(", ")));
        }
        None
    }
}

#[component]
pub fn ResolumeHealthCard() -> impl IntoView {
    let snapshot = RwSignal::new(Vec::<HostHealth>::new());

    // Cancellation flag the poll loop checks each iteration. Flipped to
    // true when the component unmounts so the spawn_local task exits
    // instead of running for the lifetime of the wasm runtime.
    let cancelled = RwSignal::new(false);
    on_cleanup(move || cancelled.set(true));

    let _poll = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            loop {
                if cancelled.get_untracked() {
                    break;
                }
                if let Ok(data) =
                    crate::api::get::<Vec<HostHealth>>("/api/v1/resolume/health").await
                {
                    snapshot.set(data);
                }
                gloo_timers::future::TimeoutFuture::new(5_000).await;
            }
        });
    });

    view! {
        <Show when=move || snapshot.get().iter().any(|h| h.problem().is_some()) fallback=|| view! {}>
            <div class="resolume-health-alert">
                <For
                    each=move || {
                        snapshot
                            .get()
                            .into_iter()
                            .filter_map(|h| h.problem().map(|p| (h.host.clone(), p)))
                            .collect::<Vec<_>>()
                    }
                    key=|(host, _)| host.clone()
                    children=move |(host, reason)| {
                        view! {
                            <div class="rh-alert">
                                <span class="rh-alert-dot"></span>
                                <strong>{format!("Resolume {host}")}</strong>
                                ": "
                                {reason}
                            </div>
                        }
                    }
                />
            </div>
        </Show>
    }
}
