//! #194 ROUND 3b: ONE status strip on every page.
//!
//! Before #194 the OBS / genlock / Resolume / tools / LAN / version badges lived
//! only on the Dashboard, each with its own wording; the other four pages showed
//! nothing but the lone WS dot + version in the navbar. `HealthBar` is the ONE
//! strip, mounted once in `app.rs` above the page switch, so the SAME segments —
//! same testids (`health-<part>`), same Slovak labels — render on every page.
//!
//! It absorbs `obs_status.rs`, the `resolume_health.rs` alert, the
//! `ndi_health.rs` header badge (rendered via [`GlobalLockBadge`], which keeps
//! the `genlock-global-badge` testid + the fleet LOCKED/DEGRADED/UNLOCKED
//! vocabulary), and `lan_address.rs`. The pure label/tone vocabulary is in
//! `sp_core::health` (unit-tested; sp-ui has no test job).

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use sp_core::health;

use crate::api;
use crate::components::ndi_health::GlobalLockBadge;
use crate::store::{DashboardStore, poll_into};

/// The subset of `/api/v1/status` the LAN segment needs. Both optional, so an
/// old server / a mock without them just renders `LAN: —`.
#[derive(Debug, Default, Clone, Deserialize)]
struct LanInfo {
    #[serde(default)]
    lan_url: Option<String>,
    #[serde(default)]
    lan_ip: Option<String>,
}

/// The port the dashboard is served on (same-origin as the server) — used for
/// the raw-IP fallback URL instead of assuming 8920.
fn current_port() -> String {
    web_sys::window()
        .and_then(|w| w.location().port().ok())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "8920".to_string())
}

#[component]
pub fn HealthBar() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    // Resolume health poll (5 s), through the shared helper. `cancelled` is
    // owned here; HealthBar never unmounts (app-root), but the pattern is kept.
    let cancelled = RwSignal::new(false);
    on_cleanup(move || cancelled.set(true));
    let _resolume_poll = Effect::new(move |_| {
        poll_into(
            "/api/v1/resolume/health",
            5_000,
            cancelled,
            store.resolume_health,
        );
    });

    // LAN address — one-shot fetch of `/api/v1/status` (no loop → never touches
    // a disposed signal after navigation, per sp-ui-frontend.md).
    let lan_url = RwSignal::new(None::<String>);
    let lan_ip = RwSignal::new(None::<String>);
    let _lan = Effect::new(move |_| {
        spawn_local(async move {
            if let Ok(info) = api::get::<LanInfo>("/api/v1/status").await {
                let _ = lan_url.try_set(info.lan_url);
                let _ = lan_ip.try_set(info.lan_ip);
            }
        });
    });

    view! {
        <div class="health-bar" data-testid="health-bar">
            // WS
            {move || {
                let (tone, text) = health::ws_label(store.ws_connected.get());
                let cls = format!("health-seg {}", tone.css_class());
                view! {
                    <span class=cls data-testid="health-ws">
                        <span class="health-dot"></span>
                        {text}
                    </span>
                }
            }}
            // OBS + active scene
            {move || {
                let scene = store.obs_scene.get();
                let (tone, text) = health::obs_label(store.obs_connected.get(), scene.as_deref());
                let cls = format!("health-seg {}", tone.css_class());
                view! { <span class=cls data-testid="health-obs">{text}</span> }
            }}
            // Genlock — the fleet whole-box summary (keeps genlock-global-badge).
            <span class="health-seg" data-testid="health-genlock">
                <GlobalLockBadge />
            </span>
            // #196: NDI post-restart self-check — how many outputs the server
            // flagged as still without a receiver after a restart. Hidden when
            // none (clears the moment they reconnect). `store.ndi_health` is
            // filled by the GlobalLockBadge poll above.
            {move || {
                let n = store
                    .ndi_health
                    .get()
                    .iter()
                    .filter(|o| {
                        o.degraded_reason.as_deref()
                            == Some(health::NO_RECEIVER_AFTER_RESTART_REASON)
                    })
                    .count();
                // `Option<impl IntoView>`: Leptos renders `Some` and nothing for
                // `None`, so the segment appears only while an output is dark
                // and clears the moment they reconnect.
                health::ndi_label(n).map(|(tone, text)| {
                    let cls = format!("health-seg {}", tone.css_class());
                    view! { <span class=cls data-testid="health-ndi">{text}</span> }
                })
            }}
            // Resolume push-chain
            {move || {
                let hosts = store.resolume_health.get();
                let problems: Vec<String> = hosts
                    .iter()
                    .filter_map(|h| h.problem().map(|p| format!("{}: {}", h.host, p)))
                    .collect();
                let (tone, text) = health::resolume_label(hosts.len(), problems.len());
                let cls = format!("health-seg {}", tone.css_class());
                let title = problems.join(" | ");
                view! { <span class=cls title=title data-testid="health-resolume">{text}</span> }
            }}
            // Tools (yt-dlp / ffmpeg / JS runtime)
            {move || {
                let t = store.tools.get();
                let (known, y, f, js) = match &t {
                    Some(ti) => (true, ti.ytdlp_available, ti.ffmpeg_available, ti.js_runtime_ok),
                    None => (false, false, false, false),
                };
                let (tone, text) = health::tools_label(known, y, f, js);
                let cls = format!("health-seg {}", tone.css_class());
                let title = t
                    .as_ref()
                    .map(|ti| {
                        format!(
                            "yt-dlp {} · deno {}",
                            ti.ytdlp_version.clone().unwrap_or_else(|| "—".into()),
                            ti.deno_version.clone().unwrap_or_else(|| "—".into()),
                        )
                    })
                    .unwrap_or_default();
                view! { <span class=cls title=title data-testid="health-tools">{text}</span> }
            }}
            // LAN address
            <span class="health-seg health-off" data-testid="health-lan">
                {move || {
                    let url = lan_url.get();
                    let ip = lan_ip.get();
                    match (url, ip) {
                        (None, None) => view! { <span>"LAN: —"</span> }.into_any(),
                        (Some(url), ip) => {
                            let href = url.clone();
                            view! {
                                <span>"LAN: "</span>
                                <a class="lan-link" href=href>{url}</a>
                                {ip.map(|ip| view! {
                                    <span class="lan-fallback">{format!(" (alebo {ip})")}</span>
                                }.into_any())}
                            }
                            .into_any()
                        }
                        (None, Some(ip)) => {
                            let fallback = format!("http://{ip}:{}", current_port());
                            let href = fallback.clone();
                            view! {
                                <span>"LAN: "</span>
                                <a class="lan-link" href=href>{fallback}</a>
                            }
                            .into_any()
                        }
                    }
                }}
            </span>
            // Version — nests the existing `version` testid (post-deploy +
            // version-assertion contract) inside the `health-version` wrapper.
            <span class="health-seg" data-testid="health-version">
                <span class="version-label" data-testid="version">
                    {format!("v{}", sp_core::config::VERSION)}
                </span>
            </span>
        </div>
    }
}
