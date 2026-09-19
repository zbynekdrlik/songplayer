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
use wasm_bindgen::prelude::*;

#[wasm_bindgen(module = "/preview_player.js")]
extern "C" {
    type PreviewPlayer;

    #[wasm_bindgen(constructor)]
    fn new(video: &web_sys::HtmlVideoElement, path: String, start_muted: bool) -> PreviewPlayer;

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
) -> impl IntoView {
    let video_ref = NodeRef::<Video>::new();
    // `StoredValue<_, LocalStorage>` (Copy + Send handle, thread-local value)
    // holds the `!Send` player across the effect, the cleanup, and the handlers.
    let player = StoredValue::new_local(None::<PreviewPlayer>);

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
            player.set_value(Some(PreviewPlayer::new(&el, path, false)));
        }
    });

    // Unmount / navigation: tear the player down (closes the WS → the encoder
    // child dies after its TTL).
    on_cleanup(move || {
        player.update_value(|p| {
            if let Some(pl) = p.take() {
                pl.destroy();
            }
        });
    });

    let on_unmute = move |_| {
        player.with_value(|p| {
            if let Some(pl) = p {
                pl.unmute();
            }
        });
    };
    let on_fullscreen = move |_| {
        player.with_value(|p| {
            if let Some(pl) = p {
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
