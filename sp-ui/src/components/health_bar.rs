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
//!
//! #230: while a `sp-90s` press holds the background jobs, an amber
//! segment says so (`sp_core::background_hold::label`), polled every 5 s.
//!
//! #233: while the box violates the production environment (an edition that
//! is neither LTSC nor IoT Enterprise, Defender's real-time protection on, a
//! firewall profile on, or a fact that cannot be read), a red segment names
//! it (`sp_core::prod_env::label`), polled every 10 s; the bar's
//! `data-environment` says what the last poll said
//! (`sp_core::prod_env::marker`). #255: the audio profile's report-only
//! facts that are not the production ones are a grey `Profil PC: N
//! poznámky` segment from the same poll (`sp_core::prod_env::notes_label`).
//!
//! #257: the whole audio chain is ONE segment right after WS (every
//! service's first glance, #248): `Zvuk: bez chyby …`, amber / red on a
//! loss naming the worst path; a click opens the per-path panel under the
//! bar (`audio_faults.rs`), polled every 10 s.
//!
//! #259: the LED wall report — `Stena: …`, green while Resolume draws on
//! every display it wants, grey while the screens are off or Resolume does
//! not run, red while a display Resolume wants is missing or it does not
//! draw on it (`sp_core::wall::label`), polled every 5 s; nothing while
//! Resolume drives no physical display. The bar's `data-wall` says what the
//! last poll said (`sp_core::wall::marker`). It only reports: the operator's
//! own setup in Resolume is the reference, nothing is ever changed. #254:
//! after the guard ended a crashed or hung Resolume the segment names that
//! end, red, until Resolume runs again (`data-guard`: the guard's phase).
//!
//! #275: while a song plays on program and no SongPlayer clip runs in
//! Resolume (the report's `songplayer_clip`), the segment names that, amber,
//! with how long (`sp_core::wall::label`; `data-clip-missing`); the bar's
//! `data-clip` is the clip state the last poll said. It never acts: clips
//! run only from the operator's Stream Deck.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use sp_core::background_hold::{self, BackgroundHold};
use sp_core::health;
use sp_core::prod_env::{self, EnvironmentStatus};
use sp_core::wall::{self, WallStatus};
use sp_core::wall_clip;

use crate::api;
use crate::components::audio_faults::{AudioFaults, AudioFaultsPanel, AudioSegment};
use crate::components::ndi_health::GlobalLockBadge;
use crate::store::{DashboardStore, poll_into};

/// The subset of `/api/v1/status` the LAN, node and paid-AI segments need.
/// All optional, so an old server / a mock without them just renders
/// `LAN: —` and no node or paid-AI segment.
#[derive(Debug, Default, Clone, Deserialize)]
struct LanInfo {
    #[serde(default)]
    lan_url: Option<String>,
    #[serde(default)]
    lan_ip: Option<String>,
    /// #229 item C: this node's exchange name.
    #[serde(default)]
    node_name: Option<String>,
    /// #229 item C: this node's paid-AI switch.
    #[serde(default)]
    paid_ai_enabled: Option<bool>,
    /// #229 item C: the kinds of work held while it is off.
    #[serde(default)]
    paid_ai_held: Vec<String>,
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
    // #230: the background hold, polled every 5 s (it arms and ends while
    // the page is open).
    let hold = RwSignal::new(BackgroundHold::default());
    let _hold_poll = Effect::new(move |_| {
        poll_into("/api/v1/background-hold", 5_000, cancelled, hold);
    });
    // #233: the production environment, polled every 10 s (the server checks
    // it every minute; a box that turns Defender on shows up with no reload).
    let environment = RwSignal::new(EnvironmentStatus::default());
    let _env_poll = Effect::new(move |_| {
        poll_into("/api/v1/environment", 10_000, cancelled, environment);
    });
    // #259: the LED wall report, polled every 5 s (the server reads the box
    // every 5 s: a screen switched off shows up with no reload).
    let wall_status = RwSignal::new(WallStatus::default());
    let _wall_poll = Effect::new(move |_| {
        poll_into("/api/v1/wall", 5_000, cancelled, wall_status);
    });
    // #275: a song plays on program (a Memo: a position tick changes
    // nothing, so the segment is not re-rendered twice a second).
    let song_on_program = Memo::new(move |_| {
        store.now_playing.with(|playing| {
            playing.values().any(|np| {
                wall_clip::song_on_program(np.state_known, np.state, np.has_now_playing_content())
            })
        })
    });
    // #257: the audio fault history, one request at a time.
    let audio = AudioFaults::new();
    audio.start(cancelled);

    // LAN address — one-shot fetch of `/api/v1/status` (no loop → never touches
    // a disposed signal after navigation, per sp-ui-frontend.md).
    let lan_url = RwSignal::new(None::<String>);
    let lan_ip = RwSignal::new(None::<String>);
    // #229 item C: the node's name and its paid-AI switch, from the same read.
    let node = RwSignal::new(None::<String>);
    let paid_ai = RwSignal::new(None::<bool>);
    let paid_held = RwSignal::new(Vec::<String>::new());
    let _lan = Effect::new(move |_| {
        spawn_local(async move {
            if let Ok(info) = api::get::<LanInfo>("/api/v1/status").await {
                let _ = lan_url.try_set(info.lan_url);
                let _ = lan_ip.try_set(info.lan_ip);
                let _ = node.try_set(health::node_label(info.node_name.as_deref()));
                let _ = paid_ai.try_set(info.paid_ai_enabled);
                let _ = paid_held.try_set(info.paid_ai_held);
            }
        });
    });

    view! {
        // #233 review finding 5: what the last environment poll said
        // (`unchecked` / `ok` / `violated`), so a spec can tell "the box
        // holds" from "the poll has not landed".
        <div
            class="health-bar"
            data-testid="health-bar"
            data-environment=move || prod_env::marker(&environment.get())
            data-wall=move || wall::marker(&wall_status.get())
            data-clip=move || wall_status.get().songplayer_clip.state.name()
        >
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
            // #257: the whole audio chain (a click opens the panel below).
            <AudioSegment state=audio />
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
            // Resolume push-chain
            {move || {
                // #262: only a host that does not answer is a problem; a
                // token the composition lacks is the tooltip's note.
                let hosts: Vec<(String, health::ResolumeHostView)> = store
                    .resolume_health
                    .get()
                    .iter()
                    .map(|h| (h.host.clone(), h.view()))
                    .collect();
                let unreachable = hosts.iter().filter(|(_, v)| v.problem.is_some()).count();
                let clipless = hosts.iter().filter(|(_, v)| !v.has_clips).count();
                let (tone, text) = health::resolume_label(hosts.len(), unreachable, clipless);
                let cls = format!("health-seg {}", tone.css_class());
                let title = health::resolume_title(&hosts);
                view! { <span class=cls title=title data-testid="health-resolume">{text}</span> }
            }}
            // #259: the LED wall against Resolume's own saved output; #254:
            // `data-guard` = the guard's phase after its last end of Arena;
            // #275: `data-clip-missing` = the segment names a missing
            // SongPlayer clip.
            {move || {
                let status = wall_status.get();
                let song = song_on_program.get();
                let state = status.state.name();
                let guard = status.guard.phase.name();
                let missing = if wall::clip_note(&status, song) { "true" } else { "false" };
                wall::label(&status, song).map(|(tone, text, tip)| {
                    let cls = format!("health-seg {}", tone.css_class());
                    view! {
                        <span
                            class=cls
                            title=tip
                            data-testid="health-wall"
                            data-state=state
                            data-guard=guard
                            data-clip-missing=missing
                        >
                            {text}
                        </span>
                    }
                    .into_any()
                })
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
            // #229 item C: the node's name, and a warning while its paid AI
            // is off (the tooltip names what waits for a peer's copy).
            {move || {
                node.get().map(|text| {
                    view! { <span class="health-seg health-off" data-testid="health-node">{text}</span> }
                        .into_any()
                })
            }}
            {move || {
                health::paid_ai_label(paid_ai.get(), &paid_held.get()).map(|(tone, text, tip)| {
                    let cls = format!("health-seg {}", tone.css_class());
                    view! { <span class=cls title=tip data-testid="health-paid-ai">{text}</span> }
                        .into_any()
                })
            }}
            // #233: only while the production environment is violated.
            {move || {
                prod_env::label(&environment.get()).map(|(tone, text, tip)| {
                    let cls = format!("health-seg {}", tone.css_class());
                    view! { <span class=cls title=tip data-testid="health-environment">{text}</span> }
                        .into_any()
                })
            }}
            // #255: the audio profile's report-only notes (grey, never a
            // problem: the Dante adapter's driver, the Windows Update driver
            // exclusion, core parking), the notes and the power scheme as
            // the tooltip.
            {move || {
                prod_env::notes_label(&environment.get()).map(|(tone, text, tip)| {
                    let cls = format!("health-seg {}", tone.css_class());
                    view! { <span class=cls title=tip data-testid="health-environment-notes">{text}</span> }
                        .into_any()
                })
            }}
            // #230: only while a sp-90s press holds the background jobs.
            {move || {
                background_hold::label(&hold.get()).map(|(tone, text, tip)| {
                    let cls = format!("health-seg {}", tone.css_class());
                    view! { <span class=cls title=tip data-testid="health-background-hold">{text}</span> }
                        .into_any()
                })
            }}
            // Version — nests the existing `version` testid (post-deploy +
            // version-assertion contract) inside the `health-version` wrapper.
            <span class="health-seg" data-testid="health-version">
                <span class="version-label" data-testid="version">
                    {format!("v{}", sp_core::config::VERSION)}
                </span>
            </span>
            // #257: the audio panel, a full-width line under the segments.
            <AudioFaultsPanel state=audio />
        </div>
    }
}
