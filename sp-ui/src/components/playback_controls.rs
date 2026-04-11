//! Play / Pause / Skip / Previous buttons and mode selector.
//!
//! Dispatches commands to the path-based REST endpoints exposed by
//! `sp-server`. Each click calls the matching `/api/v1/playback/{id}/{action}`
//! (or `/api/v1/playlists/{id}/sync`) endpoint directly — there is
//! deliberately no `/api/v1/control` umbrella route on the server.

use leptos::prelude::*;
use serde::Serialize;
use sp_core::playback::PlaybackMode;

use crate::api;

#[derive(Serialize)]
struct SetModeBody {
    mode: String,
}

#[component]
pub fn PlaybackControls(playlist_id: i64) -> impl IntoView {
    let pid = playlist_id;

    let on_play = move |_| {
        leptos::task::spawn_local(async move {
            let _ = api::post_empty(&format!("/api/v1/playback/{pid}/play")).await;
        });
    };
    let on_pause = move |_| {
        leptos::task::spawn_local(async move {
            let _ = api::post_empty(&format!("/api/v1/playback/{pid}/pause")).await;
        });
    };
    let on_skip = move |_| {
        leptos::task::spawn_local(async move {
            let _ = api::post_empty(&format!("/api/v1/playback/{pid}/skip")).await;
        });
    };
    let on_prev = move |_| {
        leptos::task::spawn_local(async move {
            let _ = api::post_empty(&format!("/api/v1/playback/{pid}/previous")).await;
        });
    };

    let on_mode = move |ev: leptos::ev::Event| {
        let val = event_target_value(&ev);
        let mode = PlaybackMode::from_str_lossy(&val);
        let body = SetModeBody {
            mode: mode.as_str().to_string(),
        };
        leptos::task::spawn_local(async move {
            let _ = api::put_json_empty(&format!("/api/v1/playback/{pid}/mode"), &body).await;
        });
    };

    let on_sync = move |_| {
        leptos::task::spawn_local(async move {
            let _ = api::post_empty(&format!("/api/v1/playlists/{pid}/sync")).await;
        });
    };

    view! {
        <div class="playback-controls">
            <button on:click=on_prev title="Previous">"Prev"</button>
            <button on:click=on_play title="Play">"Play"</button>
            <button on:click=on_pause title="Pause">"Pause"</button>
            <button on:click=on_skip title="Skip">"Skip"</button>
            <select on:change=on_mode title="Playback mode">
                <option value="continuous" selected=true>"Continuous"</option>
                <option value="single">"Single"</option>
                <option value="loop">"Loop"</option>
            </select>
            <button on:click=on_sync class="sync-btn" title="Sync playlist">"Sync"</button>
        </div>
    }
}
