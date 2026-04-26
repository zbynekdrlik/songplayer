//! Dashboard card showing Resolume push-chain health per host.
//! Polls /api/v1/resolume/health every 5s.

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

#[component]
pub fn ResolumeHealthCard() -> impl IntoView {
    let snapshot = RwSignal::new(Vec::<HostHealth>::new());

    let _poll = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            loop {
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
        <div class="resolume-health-card">
            <h3>"Resolume hosts"</h3>
            <For
                each=move || snapshot.get()
                key=|h| h.host.clone()
                children=move |host| {
                    let dot_class = if host.circuit_breaker_open {
                        "status-dot"
                    } else if host.consecutive_failures > 0
                        || host.clips_by_token.values().any(|&v| v == 0)
                    {
                        "status-dot rh-yellow"
                    } else {
                        "status-dot connected"
                    };
                    let ts = host
                        .last_refresh_ts
                        .clone()
                        .unwrap_or_else(|| "never".into());
                    let tokens: Vec<(String, usize)> =
                        host.clips_by_token.clone().into_iter().collect();
                    view! {
                        <div class="rh-host">
                            <span class=dot_class></span>
                            <strong>{host.host.clone()}</strong>
                            <span class="rh-ts">{ts}</span>
                            <span class="rh-tokens">
                                <For
                                    each=move || tokens.clone()
                                    key=|(k, _)| k.clone()
                                    children=move |(token, count)| {
                                        let cls = if count == 0 {
                                            "rh-token rh-zero"
                                        } else {
                                            "rh-token"
                                        };
                                        view! {
                                            <span class=cls>{format!("{token}={count}")}</span>
                                        }
                                    }
                                />
                            </span>
                        </div>
                    }
                }
            />
        </div>
    }
}
