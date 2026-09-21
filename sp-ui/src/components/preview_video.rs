//! #178 live A/V preview `<video>` — the wasm_bindgen wrapper over
//! `preview_player.js` (the MSE shim).
//!
//! This component is mounted by `playlist_card.rs` only after the operator
//! CLICKS the "▶ Živý náhľad" start control on a Playing card (round 3: the
//! app's own hidden Tauri webview is a permanent viewer, so an auto-mounted
//! preview ran an encoder child 24/7 while anything played — this keeps the
//! "zero cost when nobody watches" contract). Mount opens the stream from that
//! real click (a user gesture → the player starts UNMUTED); the stop control,
//! leaving the Playing state, or navigation fires `on_cleanup` which
//! `destroy()`s the player and closes the socket → the encoder child dies.
//!
//! Disposed-signal safety (sp-ui-frontend.md): the player is driven by an
//! `Effect` that reads `video_ref.get()` — an Effect is torn down with its
//! reactive owner and never runs after disposal, so there is no
//! `spawn_local`-loop-after-unmount hazard here.
//!
//! `PreviewPlayer` is a `!Send` JsValue wrapper, but leptos 0.7's reactive
//! runtime requires the values captured by `Effect`/`on_cleanup` to be `Send`.
//! So the handle lives in a `StoredValue<_, LocalStorage>` (`new_local`): the
//! stored value stays thread-local (single-threaded WASM), while the handle
//! itself is `Copy + Send` and safely captured by the effect, the cleanup, and
//! the two button handlers.

use leptos::html::Video;
use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(module = "/preview_player.js")]
extern "C" {
    type PreviewPlayer;

    // #184 round F: `on_lag` is a JS function the shim calls each pump tick with
    // the measured picture lag in seconds (produced_ms/1000 − buffered_end). It
    // is a wasm-bindgen `Closure` passed by its `JsValue` handle (sp-ui has no
    // `js_sys` dep); the Rust closure must be kept alive as long as the player is.
    #[wasm_bindgen(constructor)]
    fn new(
        video: &web_sys::HtmlVideoElement,
        path: String,
        start_muted: bool,
        on_lag: &JsValue,
    ) -> PreviewPlayer;

    #[wasm_bindgen(method)]
    fn unmute(this: &PreviewPlayer);

    #[wasm_bindgen(method)]
    fn fullscreen(this: &PreviewPlayer);

    #[wasm_bindgen(method)]
    fn destroy(this: &PreviewPlayer);
}

#[component]
pub fn PreviewVideo(
    playlist_id: i64,
    // Fired by the stop control (and — via the card — when the playlist leaves
    // the Playing state) so the parent unmounts this component; the resulting
    // `on_cleanup` tears the player + WebSocket + encoder child down.
    #[prop(into)] on_stop: Callback<()>,
    // #184 round F: called each pump tick with the picture lag in seconds. The
    // parent (`Player`) holds it in a signal and renders the "náhľad mešká N s"
    // readout when it reaches the threshold.
    #[prop(into)] on_lag: Callback<f64>,
) -> impl IntoView {
    let video_ref = NodeRef::<Video>::new();
    // `StoredValue<_, LocalStorage>` (Copy + Send handle, thread-local value)
    // holds the `!Send` player AND the `!Send` lag `Closure` (kept alive for as
    // long as the player) across the effect, the cleanup, and the handlers.
    let player = StoredValue::new_local(None::<(PreviewPlayer, Closure<dyn Fn(f64)>)>);

    // Start the MSE player once the <video> element is actually in the DOM.
    // The effect re-runs when `video_ref` becomes populated; the `is_some`
    // guard makes starting idempotent. This component only exists after the
    // operator clicked the start control, so the player starts UNMUTED (that
    // click is the user gesture; the shim falls back to muted if the browser
    // still rejects the unmuted play(), and the unmute button stays available).
    Effect::new(move |_| {
        if player.with_value(|p| p.is_some()) {
            return;
        }
        if let Some(el) = video_ref.get() {
            let path = format!("/api/v1/playback/{playlist_id}/preview.ws");
            // The lag callback: the shim invokes it (as a JS function) each pump
            // tick. Held in the StoredValue so it outlives every JS call and is
            // dropped only with the player at cleanup.
            let cb = Closure::wrap(Box::new(move |lag: f64| on_lag.run(lag)) as Box<dyn Fn(f64)>);
            let pl = PreviewPlayer::new(&el, path, false, cb.as_ref());
            player.set_value(Some((pl, cb)));
        }
    });

    // Unmount / navigation: tear the player down (closes the WS → the encoder
    // child dies after its TTL). `destroy()` clears the pump interval before the
    // lag `Closure` drops, so no callback fires after teardown.
    on_cleanup(move || {
        player.update_value(|p| {
            if let Some((pl, _cb)) = p.take() {
                pl.destroy();
            }
        });
    });

    let on_unmute = move |_| {
        player.with_value(|p| {
            if let Some((pl, _)) = p {
                pl.unmute();
            }
        });
    };
    let on_fullscreen = move |_| {
        player.with_value(|p| {
            if let Some((pl, _)) = p {
                pl.fullscreen();
            }
        });
    };
    // Tell the parent to unmount us; `on_cleanup` then destroys the player.
    let on_stop_click = move |_| on_stop.run(());

    view! {
        <div class="preview-video-box">
            <video
                class="preview-video"
                data-testid="preview-video"
                node_ref=video_ref
                prop:muted=true
                playsinline=""
            ></video>
            <div class="preview-controls">
                <button
                    type="button"
                    class="preview-btn"
                    data-testid="preview-unmute"
                    on:click=on_unmute
                >
                    "🔊 Zvuk"
                </button>
                <button
                    type="button"
                    class="preview-btn"
                    data-testid="preview-fullscreen"
                    on:click=on_fullscreen
                >
                    "⛶ Celá obrazovka"
                </button>
                <button
                    type="button"
                    class="preview-btn"
                    data-testid="preview-stop"
                    on:click=on_stop_click
                >
                    "⏹ Zastaviť náhľad"
                </button>
            </div>
        </div>
    }
}
