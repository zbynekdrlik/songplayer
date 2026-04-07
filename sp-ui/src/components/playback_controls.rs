//! Play / Pause / Skip / Previous buttons and mode selector.
//!
//! Sends [`ClientMsg`] commands over the REST API (the WebSocket write-half
//! is read-only for now; commands go through POST endpoints).

use leptos::prelude::*;
use sp_core::playback::PlaybackMode;
use sp_core::ws::ClientMsg;

use crate::api;

#[component]
pub fn PlaybackControls(playlist_id: i64) -> impl IntoView {
    let pid = playlist_id;

    let send_cmd = move |msg: ClientMsg| {
        leptos::task::spawn_local(async move {
            let _ = api::post_json::<ClientMsg, serde_json::Value>("/api/v1/control", &msg).await;
        });
    };

    let on_play = move |_| send_cmd(ClientMsg::Play { playlist_id: pid });
    let on_pause = move |_| send_cmd(ClientMsg::Pause { playlist_id: pid });
    let on_skip = move |_| send_cmd(ClientMsg::Skip { playlist_id: pid });
    let on_prev = move |_| {
        send_cmd(ClientMsg::Previous { playlist_id: pid });
    };

    let on_mode = move |ev: leptos::ev::Event| {
        let val = event_target_value(&ev);
        let mode = PlaybackMode::from_str_lossy(&val);
        send_cmd(ClientMsg::SetMode {
            playlist_id: pid,
            mode,
        });
    };

    let on_sync = move |_| {
        send_cmd(ClientMsg::SyncPlaylist { playlist_id: pid });
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
