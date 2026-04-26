//! Dashboard alert for NDI delivery health.
//!
//! Quiet by default — renders nothing when every pipeline is healthy.
//! Mirrors `resolume_health.rs`'s alert-only pattern: dashboard noise is
//! the enemy; show only when a real problem exists.
//!
//! Polls `/api/v1/ndi/health` every 5 s.

use leptos::prelude::*;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct PipelineHealth {
    pub playlist_id: i64,
    pub ndi_name: String,
    pub state: String,
    pub connections: i32,
    pub observed_fps: f32,
    pub nominal_fps: f32,
    pub last_submit_ts: Option<String>,
    pub consecutive_bad_polls: u32,
    pub frames_submitted_total: u64,
    pub frames_submitted_last_5s: u32,
    pub degraded_reason: Option<String>,
}

impl PipelineHealth {
    /// Short human reason this pipeline is degraded, or `None` if healthy.
    /// Server fills `degraded_reason` when consecutive_bad_polls >= 2;
    /// frontend renders it verbatim and falls back to None otherwise.
    fn problem(&self) -> Option<String> {
        if self.state != "Playing" {
            return None;
        }
        if self.consecutive_bad_polls < 2 {
            return None;
        }
        self.degraded_reason.clone()
    }
}

#[component]
pub fn NdiHealthCard() -> impl IntoView {
    let snapshot = RwSignal::new(Vec::<PipelineHealth>::new());

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
                    crate::api::get::<Vec<PipelineHealth>>("/api/v1/ndi/health").await
                {
                    snapshot.set(data);
                }
                gloo_timers::future::TimeoutFuture::new(5_000).await;
            }
        });
    });

    view! {
        <Show when=move || snapshot.get().iter().any(|h| h.problem().is_some()) fallback=|| view! {}>
            <div class="ndi-health-alert">
                <For
                    each=move || {
                        snapshot
                            .get()
                            .into_iter()
                            .filter_map(|h| h.problem().map(|p| (h.playlist_id, h.ndi_name.clone(), p)))
                            .collect::<Vec<_>>()
                    }
                    key=|(id, _, _)| *id
                    children=move |(_, ndi_name, reason)| {
                        view! {
                            <div class="nh-alert">
                                <span class="nh-alert-dot"></span>
                                <strong>{format!("NDI {ndi_name}")}</strong>
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
