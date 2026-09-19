//! #178 live A/V preview `<video>` — the wasm_bindgen wrapper over
//! `preview_player.js` (the MSE shim).
//!
//! This component is mounted by `playlist_card.rs` ONLY while the card is
//! Playing (the placeholder shows otherwise), so the preview WebSocket +
//! encoder child exist strictly on demand: mount opens the stream, unmount
//! (playback stops, or navigation) fires `on_cleanup` which `destroy()`s the
//! player and closes the socket.
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
    fn new(video: &web_sys::HtmlVideoElement, path: String) -> PreviewPlayer;

    #[wasm_bindgen(method)]
    fn unmute(this: &PreviewPlayer);

    #[wasm_bindgen(method)]
    fn fullscreen(this: &PreviewPlayer);

    #[wasm_bindgen(method)]
    fn destroy(this: &PreviewPlayer);
}

#[component]
pub fn PreviewVideo(playlist_id: i64) -> impl IntoView {
    let video_ref = NodeRef::<Video>::new();
    // `StoredValue<_, LocalStorage>` (Copy + Send handle, thread-local value)
    // holds the `!Send` player across the effect, the cleanup, and the handlers.
    let player = StoredValue::new_local(None::<PreviewPlayer>);

    // Start the MSE player once the <video> element is actually in the DOM.
    // The effect re-runs when `video_ref` becomes populated; the `is_some`
    // guard makes starting idempotent.
    Effect::new(move |_| {
        if player.with_value(|p| p.is_some()) {
            return;
        }
        if let Some(el) = video_ref.get() {
            let path = format!("/api/v1/playback/{playlist_id}/preview.ws");
            player.set_value(Some(PreviewPlayer::new(&el, path)));
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
            </div>
        </div>
    }
}
