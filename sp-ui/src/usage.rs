//! #256: the dashboard records how it is used, the measured basis of the
//! #248 redesign. ONE capture-phase listener on `document` for each of
//! click / change / submit (so every control is covered, a new one too,
//! with no per-component code), plus a `page` event per route change.
//!
//! - An event names its page (the router's path), its control (the nearest
//!   `data-testid`, else `<tag>`), its kind and `Date.now()`
//!   (`sp_core::ui_usage::control_name`, `UsageEvent::new`).
//! - The events wait in a buffer (`sp_core::ui_usage::UsageBuffer`) and go
//!   to `POST /api/v1/ui-usage` every 10 s when there are any, at once when
//!   200 are waiting, and by `navigator.sendBeacon` when the page is hidden
//!   (a tab closed or switched away).
//! - Nothing is installed in an automated session (`navigator.webdriver`,
//!   which every Playwright run sets, the post-deploy specs on the boxes
//!   among them) or in a browser with the `sp_no_usage` localStorage key;
//!   both are read once, here.
//! - A send that fails is dropped: the log is statistics, and the operator
//!   never sees an error for it (this module writes no console line).
//!
//! The rules are pure and tested in `sp_core::ui_usage` (sp-ui has no unit
//! tests); this module only wires the browser. Installed once by `App`,
//! whose root never unmounts, so the listeners and the timer live for the
//! page's life (`forget`).

use std::cell::RefCell;

use leptos::prelude::*;
use sp_core::ui_usage::{self, UsageBatch, UsageBuffer, UsageEvent, UsageKind};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;

use crate::app::Page;

#[wasm_bindgen(module = "/usage_env.js")]
extern "C" {
    /// `navigator.webdriver === true`.
    fn webdriver() -> bool;

    /// `Date.now()`.
    #[wasm_bindgen(js_name = dateNow)]
    fn date_now() -> f64;
}

thread_local! {
    /// The events not sent yet.
    static BUFFER: RefCell<UsageBuffer> = RefCell::new(UsageBuffer::default());
}

/// Install the usage log (once, from `App`). `page` is the router's page.
pub fn install(page: RwSignal<Page>) {
    if ui_usage::excluded(webdriver(), no_usage_key().as_deref()) {
        return;
    }
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    for kind in [UsageKind::Click, UsageKind::Change, UsageKind::Submit] {
        let listener = Closure::wrap(Box::new(move |ev: web_sys::Event| {
            record_target(kind, &ev, page);
        }) as Box<dyn FnMut(web_sys::Event)>);
        // Capture phase: the event is recorded before any control's own
        // handler runs, so a click that changes the page names the page it
        // was made on.
        let _ = document.add_event_listener_with_callback_and_bool(
            kind.as_str(),
            listener.as_ref().unchecked_ref(),
            true,
        );
        listener.forget();
    }
    let on_visibility = Closure::wrap(Box::new(move |_ev: web_sys::Event| {
        let hidden = web_sys::window()
            .and_then(|w| w.document())
            .is_some_and(|d| d.hidden());
        if hidden {
            send_beacon();
        }
    }) as Box<dyn FnMut(web_sys::Event)>);
    let _ = document.add_event_listener_with_callback(
        "visibilitychange",
        on_visibility.as_ref().unchecked_ref(),
    );
    on_visibility.forget();
    gloo_timers::callback::Interval::new(ui_usage::FLUSH_EVERY_MS, send_buffered).forget();
    // One `page` event per route change; the first run records the page the
    // load opened. A tab clicked while already shown changes nothing.
    Effect::new(move |previous: Option<Page>| -> Page {
        let shown = page.get();
        if previous != Some(shown) {
            record(UsageEvent::new(
                now_ms(),
                shown.as_path(),
                "",
                UsageKind::Page,
            ));
        }
        shown
    });
}

/// The `sp_no_usage` localStorage key's value, if the key is there.
fn no_usage_key() -> Option<String> {
    let storage = web_sys::window()?.local_storage().ok()??;
    storage.get_item(ui_usage::NO_USAGE_KEY).ok()?
}

fn now_ms() -> i64 {
    date_now() as i64
}

/// Record a `kind` event on the element `ev` reached.
fn record_target(kind: UsageKind, ev: &web_sys::Event, page: RwSignal<Page>) {
    let Some(element) = ev
        .target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
    else {
        return;
    };
    let testid = element
        .closest("[data-testid]")
        .ok()
        .flatten()
        .and_then(|found| found.get_attribute("data-testid"));
    let control = ui_usage::control_name(testid.as_deref(), &element.tag_name());
    let Some(shown) = page.try_get_untracked() else {
        return;
    };
    record(UsageEvent::new(now_ms(), shown.as_path(), &control, kind));
}

fn record(event: UsageEvent) {
    let full = BUFFER.with(|buffer| buffer.borrow_mut().push(event));
    if let Some(batch) = full {
        send(batch);
    }
}

/// The 10 s flush: whatever was recorded since the last one.
fn send_buffered() {
    if let Some(batch) = BUFFER.with(|buffer| buffer.borrow_mut().take()) {
        send(batch);
    }
}

fn send(batch: UsageBatch) {
    leptos::task::spawn_local(async move {
        if let Ok(request) = gloo_net::http::Request::post(ui_usage::INGEST_PATH).json(&batch) {
            // A lost batch is statistics, never an error to show.
            let _ = request.send().await;
        }
    });
}

/// The page is being hidden (closed, or another tab): hand what is waiting
/// to the browser, which sends it even after the page is gone.
fn send_beacon() {
    let Some(batch) = BUFFER.with(|buffer| buffer.borrow_mut().take()) else {
        return;
    };
    let Ok(body) = serde_json::to_string(&batch) else {
        return;
    };
    if let Some(window) = web_sys::window() {
        let _ = window
            .navigator()
            .send_beacon_with_opt_str(ui_usage::INGEST_PATH, Some(&body));
    }
}
