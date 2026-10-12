//! #257 lane 2: the audio fault history on every page.
//!
//! The owner (12.10.2026): "song player zobrazoval pocet problemov a nejaky
//! cas/graph aby bolo videt ako dlho to je bez chyby alebo ked sa problem
//! udial", for every audio path ("akékoľvek puknutie, strata samplu je
//! neakceptovateľná"). Placed by usage frequency (#248 ROZHODNUTÉ): the
//! whole chain is ONE HealthBar segment at first glance (`health-audio`,
//! its `data-level` unknown | ok | risk | loss | recent); one click opens
//! the panel (`audio-faults-panel`): a row per path with its counts, its
//! "bez chyby od" and a timeline on one shared time axis, then the event
//! list, over 1 h / 24 h / 7 d / 30 d.
//!
//! ONE poll ([`AudioFaults::start`]) asks `GET /api/v1/audio/faults`, one
//! request at a time (`sp_core::audio_faults::next_fetch`): the segment's
//! 24 h every 10 s, the panel's own window while it is open. Every text,
//! level, bucket and tick is `sp_core`'s (`audio_faults`,
//! `audio_faults_timeline`, unit-tested); this file only renders. Local
//! times come from the browser's `Date` (its zone's own rules); every
//! "ago" counts from the server's `now_ms`.

use std::collections::{BTreeMap, HashMap};

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use sp_core::audio_faults::{
    self as faults, FaultEvent, FaultsAnswer, Fetch, LocalTime, PathFaults, Polls, SEGMENT_HOURS,
    WINDOW_HOURS,
};
use sp_core::audio_faults_timeline::{
    self as timeline, BAR_HEIGHT, BAR_WIDTH, BUCKETS, Bucket, SLOT,
};
use sp_core::models::Playlist;
use wasm_bindgen::prelude::*;

use crate::api;
use crate::components::player::now_ms;
use crate::store::DashboardStore;

#[wasm_bindgen]
extern "C" {
    /// JavaScript's `Date`: it reads an instant in the browser's own time
    /// zone, with that zone's daylight-saving rules.
    type Date;

    #[wasm_bindgen(constructor)]
    fn new(ms: f64) -> Date;

    #[wasm_bindgen(method, js_name = getDate)]
    fn get_date(this: &Date) -> f64;

    #[wasm_bindgen(method, js_name = getMonth)]
    fn get_month(this: &Date) -> f64;

    #[wasm_bindgen(method, js_name = getHours)]
    fn get_hours(this: &Date) -> f64;

    #[wasm_bindgen(method, js_name = getMinutes)]
    fn get_minutes(this: &Date) -> f64;

    #[wasm_bindgen(method, js_name = getSeconds)]
    fn get_seconds(this: &Date) -> f64;

    #[wasm_bindgen(method, js_name = getTimezoneOffset)]
    fn get_timezone_offset(this: &Date) -> f64;
}

/// An instant (UTC ms) in the browser's local time.
fn local(utc_ms: i64) -> LocalTime {
    let d = Date::new(utc_ms as f64);
    LocalTime {
        day: d.get_date() as u32,
        month: d.get_month() as u32 + 1,
        hour: d.get_hours() as u32,
        minute: d.get_minutes() as u32,
        second: d.get_seconds() as u32,
    }
}

/// The browser's zone offset at `utc_ms`, minutes east of UTC.
fn offset_east(utc_ms: i64) -> i32 {
    -(Date::new(utc_ms as f64).get_timezone_offset() as i32)
}

/// The outputs' sample rates, from `GET /api/v1/program` (only what the
/// panel needs; everything else is skipped).
#[derive(Debug, Default, Deserialize)]
struct ProgramRates {
    #[serde(default)]
    outputs: Vec<OutputRate>,
}

#[derive(Debug, Deserialize)]
struct OutputRate {
    id: String,
    #[serde(default)]
    rate: u32,
    #[serde(default)]
    asio: Option<AsioRate>,
}

#[derive(Debug, Deserialize)]
struct AsioRate {
    #[serde(default)]
    driver_rate: u32,
}

/// The HealthBar's audio state: the segment's day, the panel's window and
/// its answer, the outputs' rates.
#[derive(Clone, Copy)]
pub struct AudioFaults {
    /// The [`SEGMENT_HOURS`] answer (the segment's, and the panel's at
    /// that window).
    day: RwSignal<Option<FaultsAnswer>>,
    /// The panel's own window's last answer.
    window_answer: RwSignal<Option<FaultsAnswer>>,
    open: RwSignal<bool>,
    /// The panel's window (hours).
    window: RwSignal<u32>,
    /// Each output's sample rate, by id (read when the panel opens).
    rates: RwSignal<BTreeMap<String, u32>>,
}

impl AudioFaults {
    pub fn new() -> Self {
        Self {
            day: RwSignal::new(None),
            window_answer: RwSignal::new(None),
            open: RwSignal::new(false),
            window: RwSignal::new(SEGMENT_HOURS),
            rates: RwSignal::new(BTreeMap::new()),
        }
    }

    /// The ONE poll, one request at a time: what `next_fetch` names, else a
    /// 250 ms nap (a window the panel switches to is asked within it).
    /// Page-owned signals are read with `try_*` (`sp-ui-frontend.md`).
    pub fn start(self, cancelled: RwSignal<bool>) {
        spawn_local(async move {
            let mut polls = Polls::default();
            loop {
                if cancelled.try_get_untracked() != Some(false) {
                    break;
                }
                let (Some(open), Some(hours)) = (
                    self.open.try_get_untracked(),
                    self.window.try_get_untracked(),
                ) else {
                    break;
                };
                let now = now_ms();
                match faults::next_fetch(polls, now, open.then_some(hours)) {
                    Some(Fetch::Day) => {
                        polls.day_at = Some(now);
                        let url = faults::faults_url(SEGMENT_HOURS);
                        if let Ok(answer) = api::get::<FaultsAnswer>(&url).await
                            && self.day.try_set(Some(answer)).is_some()
                        {
                            break;
                        }
                    }
                    Some(Fetch::Window(h)) => {
                        polls.window = Some((h, now));
                        let url = faults::faults_url(h);
                        if let Ok(answer) = api::get::<FaultsAnswer>(&url).await
                            && self.window_answer.try_set(Some(answer)).is_some()
                        {
                            break;
                        }
                    }
                    None => gloo_timers::future::TimeoutFuture::new(250).await,
                }
            }
        });
    }
}

/// A playlist's name by id, from the loaded playlists.
fn playlist_name(playlists: &[Playlist]) -> impl Fn(i64) -> Option<String> + '_ {
    move |id| {
        playlists
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
    }
}

/// The first glance: ONE segment for the whole audio chain. A click opens
/// (and closes) the panel.
#[component]
pub fn AudioSegment(state: AudioFaults) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");
    let segment = Memo::new(move |_| {
        let playlists = store.playlists.get();
        let name = playlist_name(&playlists);
        state.day.with(|a| faults::segment(a.as_ref(), &name))
    });
    view! {
        <button
            type="button"
            class=move || {
                format!("health-seg health-audio {}", segment.with(|s| s.level.tone().css_class()))
            }
            data-testid="health-audio"
            data-level=move || segment.with(|s| s.level.marker())
            title=move || segment.with(|s| s.tip.clone())
            aria-expanded=move || state.open.get().to_string()
            on:click=move |_| state.open.update(|open| *open = !*open)
        >
            {move || segment.with(|s| s.text.clone())}
            {move || {
                segment
                    .with(|s| s.risks.clone())
                    .map(|risks| {
                        view! {
                            <span class="health-audio-risks" data-testid="health-audio-risks">
                                {risks}
                            </span>
                        }
                    })
            }}
        </button>
    }
}

/// One line of the event list, built once per event (the `<For>` key holds
/// everything it shows).
#[derive(Clone, Debug, PartialEq)]
struct EventRow {
    key: String,
    ts_ms: i64,
    path: String,
    severity: String,
    time: String,
    name: String,
    kind: String,
    amount: String,
}

/// The event list of `a`, newest first.
fn event_rows(
    a: &FaultsAnswer,
    names: &dyn Fn(i64) -> Option<String>,
    rates: &BTreeMap<String, u32>,
) -> Vec<EventRow> {
    let rate_by_output = |id: &str| rates.get(id).copied();
    let mut seen: HashMap<String, usize> = HashMap::new();
    faults::listed_events(a)
        .into_iter()
        .map(|(p, e)| {
            let rate = faults::rate_of(&p.path, &rate_by_output);
            let name = faults::name_of(p, names);
            let amount = faults::amount_sk(&e.kind, e.amount, rate).unwrap_or_default();
            let base = format!(
                "{}|{}|{}|{}|{}|{}",
                e.ts_ms, p.path, e.kind, e.severity, amount, name
            );
            let n = seen.entry(base.clone()).or_default();
            *n += 1;
            EventRow {
                key: format!("{base}#{n}"),
                ts_ms: e.ts_ms,
                path: p.path.clone(),
                severity: e.severity.clone(),
                time: faults::time_sk(local(e.ts_ms)),
                name,
                kind: faults::kind_sk(&e.kind).to_string(),
                amount,
            }
        })
        .collect()
}

/// A path's timeline: one bar per bucket that holds a fault, to scale,
/// and a dashed mark at each (re)start (a planned restart is no fault).
fn timeline_svg(a: &FaultsAnswer, events: &[FaultEvent], buckets: &[Bucket], max: u32) -> AnyView {
    let width = BUCKETS as u32 * SLOT;
    let marks = timeline::start_marks(events, a.from_ms, a.now_ms)
        .into_iter()
        .map(|m| {
            let x = m.pct * f64::from(width) / 100.0;
            let tip = timeline::start_tip(local(m.at_ms));
            view! {
                <g data-testid="audio-start-mark" data-ts=m.at_ms>
                    <title>{tip}</title>
                    <line
                        class="audio-start-mark"
                        x1=x
                        y1=0
                        x2=x
                        y2=BAR_HEIGHT
                        vector-effect="non-scaling-stroke"
                    />
                </g>
            }
        })
        .collect_view();
    let bars = buckets
        .iter()
        .enumerate()
        .filter(|(_, b)| b.loss + b.risk > 0)
        .map(|(i, b)| {
            let (loss_h, risk_h) = timeline::bar_heights(*b, max);
            let x = timeline::bar_x(i);
            let loss_y = BAR_HEIGHT - loss_h;
            let risk_y = loss_y - risk_h;
            let start = timeline::bucket_start(a.from_ms, a.now_ms, BUCKETS, i);
            let end = timeline::bucket_start(a.from_ms, a.now_ms, BUCKETS, i + 1);
            let tip = timeline::bucket_tip(local(start), local(end), *b);
            view! {
                <g data-testid="audio-bar" data-loss=b.loss data-risk=b.risk>
                    <title>{tip}</title>
                    <rect class="audio-bar-loss" x=x y=loss_y width=BAR_WIDTH height=loss_h />
                    <rect class="audio-bar-risk" x=x y=risk_y width=BAR_WIDTH height=risk_h />
                </g>
            }
        })
        .collect_view();
    view! {
        <svg
            class="audio-timeline"
            data-testid="audio-path-timeline"
            viewBox=format!("0 0 {width} {BAR_HEIGHT}")
            preserveAspectRatio="none"
        >
            <line class="audio-timeline-base" x1=0 y1=BAR_HEIGHT x2=width y2=BAR_HEIGHT />
            {bars}
            {marks}
        </svg>
    }
    .into_any()
}

/// One path's row: its name, counts, "bez chyby od", the loop's time, and
/// its timeline.
fn path_row(
    a: &FaultsAnswer,
    p: &PathFaults,
    buckets: &[Bucket],
    max: u32,
    names: &dyn Fn(i64) -> Option<String>,
) -> AnyView {
    let name = faults::name_of(p, names);
    let not_running = (!p.live).then_some(" (nebeží)");
    let losses_class = if p.losses > 0 {
        "audio-path-losses has-loss"
    } else {
        "audio-path-losses"
    };
    let risks_class = if p.risks > 0 {
        "audio-path-risks has-risk"
    } else {
        "audio-path-risks"
    };
    let losses = faults::losses_sk(p.losses);
    let risks = faults::risks_sk(p.risks);
    let clean = faults::clean_sk(p.clean_since_ms, a, local(p.clean_since_ms));
    let loop_line = p.loop_check.as_ref().map(faults::loop_sk);
    let svg = timeline_svg(a, &p.events, buckets, max);
    view! {
        <div class="audio-path-row" data-testid="audio-path-row" data-path=p.path.clone()>
            <div class="audio-path-info">
                <div class="audio-path-name" data-testid="audio-path-name">
                    {name}
                    {not_running}
                </div>
                <div class="audio-path-facts">
                    <span class=losses_class data-testid="audio-path-losses">
                        {losses}
                    </span>
                    " · "
                    <span class=risks_class data-testid="audio-path-risks">
                        {risks}
                    </span>
                </div>
                <div class="audio-path-clean" data-testid="audio-path-clean">
                    {clean}
                </div>
                {loop_line
                    .map(|line| {
                        view! {
                            <div class="audio-path-loop" data-testid="audio-path-loop">
                                {line}
                            </div>
                        }
                    })}
            </div>
            {svg}
        </div>
    }
    .into_any()
}

/// Every path's row, the bars on the panel's one scale.
fn path_rows(a: &FaultsAnswer, names: &dyn Fn(i64) -> Option<String>) -> AnyView {
    let paths = faults::ordered_paths(a);
    let buckets: Vec<Vec<Bucket>> = paths
        .iter()
        .map(|p| timeline::buckets(&p.events, a.from_ms, a.now_ms, BUCKETS))
        .collect();
    let max = timeline::shared_max(&buckets);
    paths
        .iter()
        .zip(buckets.iter())
        .map(|(p, b)| path_row(a, p, b, max, names))
        .collect_view()
        .into_any()
}

/// The shared time axis under the rows, its ticks on whole local times.
fn axis(a: &FaultsAnswer) -> AnyView {
    let ticks = timeline::axis_ticks(a.from_ms, a.now_ms, offset_east(a.now_ms))
        .into_iter()
        .map(|t| {
            let at = local(t.at_ms);
            let label = if t.days {
                faults::dm_sk(at)
            } else {
                faults::hm_sk(at)
            };
            let left = format!(
                "left: {:.3}%",
                timeline::axis_pct(t.at_ms, a.from_ms, a.now_ms)
            );
            view! {
                <span class="audio-axis-tick" data-testid="audio-axis-tick" style=left>
                    {label}
                </span>
            }
        })
        .collect_view();
    view! {
        <div class="audio-axis-row">
            <div class="audio-axis-spacer"></div>
            <div class="audio-axis" data-testid="audio-axis">
                {ticks}
            </div>
        </div>
    }
    .into_any()
}

/// The open panel: its head (the window selector, a close button), the
/// rows, the shared axis, the event list.
fn panel_view(
    state: AudioFaults,
    store: DashboardStore,
    shown: Memo<Option<FaultsAnswer>>,
    events: Memo<Vec<EventRow>>,
) -> impl IntoView {
    let window_buttons = WINDOW_HOURS
        .iter()
        .map(|&hours| {
            view! {
                <button
                    type="button"
                    class=move || {
                        if state.window.get() == hours { "audio-window selected" } else { "audio-window" }
                    }
                    data-testid="audio-window"
                    data-hours=hours
                    aria-pressed=move || { (state.window.get() == hours).to_string() }
                    on:click=move |_| state.window.set(hours)
                >
                    {faults::window_label(hours)}
                </button>
            }
        })
        .collect_view();
    view! {
        <div class="audio-faults-panel" data-testid="audio-faults-panel">
            <div class="audio-faults-head">
                <strong>"Zvuk — história chýb"</strong>
                <span class="audio-faults-windows">{window_buttons}</span>
                <button
                    type="button"
                    class="audio-faults-close"
                    data-testid="audio-faults-close"
                    title="Zavrieť"
                    on:click=move |_| state.open.set(false)
                >
                    "✕"
                </button>
            </div>
            {move || {
                shown
                    .with(Option::is_none)
                    .then(|| {
                        view! {
                            <p class="audio-faults-note" data-testid="audio-faults-loading">
                                "Načítavam…"
                            </p>
                        }
                    })
            }}
            <div class="audio-paths">
                {move || {
                    let playlists = store.playlists.get();
                    let name = playlist_name(&playlists);
                    shown.with(|a| a.as_ref().map(|a| path_rows(a, &name)))
                }}
            </div>
            {move || shown.with(|a| a.as_ref().map(axis))}
            {move || {
                (shown.with(Option::is_some) && events.with(Vec::is_empty))
                    .then(|| {
                        view! {
                            <p class="audio-faults-note" data-testid="audio-faults-no-events">
                                "V tomto období žiadna chyba ani riziko."
                            </p>
                        }
                    })
            }}
            <ol class="audio-events" data-testid="audio-faults-events">
                <For
                    each=move || events.get()
                    key=|e: &EventRow| e.key.clone()
                    children=move |e: EventRow| {
                        let class = format!("audio-event audio-event-{}", e.severity);
                        view! {
                            <li
                                class=class
                                data-testid="audio-fault-event"
                                data-ts=e.ts_ms
                                data-path=e.path
                                data-severity=e.severity
                            >
                                <span class="audio-event-time" data-testid="audio-event-time">
                                    {e.time}
                                </span>
                                <span class="audio-event-path" data-testid="audio-event-path">
                                    {e.name}
                                </span>
                                <span class="audio-event-kind" data-testid="audio-event-kind">
                                    {e.kind}
                                </span>
                                <span class="audio-event-amount" data-testid="audio-event-amount">
                                    {e.amount}
                                </span>
                            </li>
                        }
                    }
                />
            </ol>
            {move || {
                shown
                    .with(|a| a.as_ref().is_some_and(|a| a.truncated))
                    .then(|| {
                        view! {
                            <p class="audio-faults-note" data-testid="audio-faults-truncated">
                                "Zoznam ukazuje len najnovšie záznamy; počty a čas bez chyby platia pre celé obdobie."
                            </p>
                        }
                    })
            }}
        </div>
    }
}

/// One click away: the panel, under the HealthBar's segments while open.
#[component]
pub fn AudioFaultsPanel(state: AudioFaults) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");
    // The outputs' rates, read each time the panel opens (an event's
    // frames read as ms at its output's rate).
    let _rates = Effect::new(move |_| {
        if state.open.get() {
            spawn_local(async move {
                if let Ok(program) = api::get::<ProgramRates>("/api/v1/program").await {
                    let rates: BTreeMap<String, u32> = program
                        .outputs
                        .into_iter()
                        .filter_map(|o| {
                            faults::output_rate(o.rate, o.asio.map(|a| a.driver_rate))
                                .map(|rate| (o.id, rate))
                        })
                        .collect();
                    let _ = state.rates.try_set(rates);
                }
            });
        }
    });
    // The answer the panel shows: its window's (the segment's at 24 h).
    let shown = Memo::new(move |_| {
        let hours = state.window.get();
        if hours == SEGMENT_HOURS {
            state.day.get()
        } else {
            state.window_answer.get().filter(|a| a.hours == hours)
        }
    });
    let events = Memo::new(move |_| {
        let playlists = store.playlists.get();
        let name = playlist_name(&playlists);
        let rates = state.rates.get();
        shown
            .with(|a| a.as_ref().map(|a| event_rows(a, &name, &rates)))
            .unwrap_or_default()
    });
    view! { {move || state.open.get().then(|| panel_view(state, store, shown, events))} }
}
