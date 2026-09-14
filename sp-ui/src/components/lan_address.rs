//! LAN `sp.local` address indicator (#51).
//!
//! Shows the offline-LAN URL the dashboard is reachable at
//! (`http://sp.local:8920`) plus the raw IP as a fallback, sourced from
//! `/api/v1/status`'s `lan_url` / `lan_ip` fields. One-shot fetch on mount —
//! no poll loop, so it never touches a disposed signal after navigation
//! (see `.claude/rules/sp-ui-frontend.md`). Renders nothing until the LAN
//! address is known / when the feature is disabled.

use leptos::prelude::*;
use serde::Deserialize;

use crate::api;

/// The subset of `/api/v1/status` this component needs. Both fields are
/// optional and default to `None` when absent, so an old server / the mock
/// without them just renders nothing.
#[derive(Debug, Default, Clone, Deserialize)]
struct LanInfo {
    #[serde(default)]
    lan_url: Option<String>,
    #[serde(default)]
    lan_ip: Option<String>,
}

/// The port the dashboard is currently served on. The dashboard is same-origin
/// as the server, so this is the operator's actual `api_port` — used to build
/// the raw-IP fallback URL instead of assuming the default 8920.
fn current_port() -> String {
    web_sys::window()
        .and_then(|w| w.location().port().ok())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "8920".to_string())
}

#[component]
pub fn LanAddress() -> impl IntoView {
    let lan_url = RwSignal::new(None::<String>);
    let lan_ip = RwSignal::new(None::<String>);

    let _load = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(info) = api::get::<LanInfo>("/api/v1/status").await {
                // `try_set` no-ops if the component unmounted mid-fetch, so a
                // late response can never panic on a disposed signal.
                let _ = lan_url.try_set(info.lan_url);
                let _ = lan_ip.try_set(info.lan_ip);
            }
        });
    });

    view! {
        <div class="lan-address" data-testid="lan-address">
            {move || {
                let url = lan_url.get();
                let ip = lan_ip.get();
                match (url, ip) {
                    // Feature off / not up yet — render nothing.
                    (None, None) => view! { <span></span> }.into_any(),
                    // mDNS advertised: sp.local URL primary, raw IP fallback.
                    (Some(url), ip) => {
                        let href = url.clone();
                        view! {
                            <span class="lan-label">"LAN: "</span>
                            <a class="lan-link" href=href>{url}</a>
                            {ip.map(|ip| view! {
                                <span class="lan-fallback">{format!(" (alebo {ip})")}</span>
                            }.into_any())}
                        }
                        .into_any()
                    }
                    // mDNS not up but the LAN IP is known — show the raw fallback
                    // on the port the dashboard is actually served on.
                    (None, Some(ip)) => {
                        let fallback = format!("http://{ip}:{}", current_port());
                        let href = fallback.clone();
                        view! {
                            <span class="lan-label">"LAN: "</span>
                            <a class="lan-link" href=href>{fallback}</a>
                        }
                        .into_any()
                    }
                }
            }}
        </div>
    }
}
