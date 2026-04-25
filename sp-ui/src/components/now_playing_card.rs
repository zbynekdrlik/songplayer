//! Mobile-friendly "now playing" card for /live. Shows song/artist,
//! scrubber, progress bar, and transport buttons. Large touch targets
//! (48 px) for phone use.

use leptos::prelude::*;

use crate::store::DashboardStore;

#[component]
pub fn NowPlayingCard(playlist_id: i64, store: DashboardStore) -> impl IntoView {
    // NowPlayingInfo fields are all non-Option: song: String, artist: String,
    // position_ms: u64, duration_ms: u64.
    let song = move || {
        store
            .now_playing
            .get()
            .get(&playlist_id)
            .map(|n| n.song.clone())
            .unwrap_or_default()
    };
    let artist = move || {
        store
            .now_playing
            .get()
            .get(&playlist_id)
            .map(|n| n.artist.clone())
            .unwrap_or_default()
    };
    let duration = move || {
        store
            .now_playing
            .get()
            .get(&playlist_id)
            .map(|n| n.duration_ms)
            .unwrap_or(0)
    };
    let position = move || {
        store
            .now_playing
            .get()
            .get(&playlist_id)
            .map(|n| n.position_ms)
            .unwrap_or(0)
    };
    let progress_pct = move || {
        let d = duration();
        if d == 0 {
            0.0_f64
        } else {
            (position() as f64 / d as f64) * 100.0
        }
    };

    let do_seek = move |ms: u64| {
        leptos::task::spawn_local(async move {
            let _ = crate::api::seek_playlist(playlist_id, ms).await;
        });
    };
    let do_play = move || {
        leptos::task::spawn_local(async move {
            let _ =
                crate::api::post_empty(&format!("/api/v1/playback/{playlist_id}/play")).await;
        });
    };
    let do_pause = move || {
        leptos::task::spawn_local(async move {
            let _ =
                crate::api::post_empty(&format!("/api/v1/playback/{playlist_id}/pause")).await;
        });
    };
    let do_skip = move || {
        leptos::task::spawn_local(async move {
            let _ =
                crate::api::post_empty(&format!("/api/v1/playback/{playlist_id}/skip")).await;
        });
    };
    let do_previous = move || {
        leptos::task::spawn_local(async move {
            let _ =
                crate::api::post_empty(&format!("/api/v1/playback/{playlist_id}/previous"))
                    .await;
        });
    };

    view! {
        <div class="now-playing-card">
            <div class="np-song">{song}</div>
            <div class="np-artist">{artist}</div>
            <div class="np-time">
                {move || fmt_ms(position())}" / "{move || fmt_ms(duration())}
            </div>
            <input
                type="range"
                class="np-scrubber"
                min="0"
                max=move || duration().to_string()
                step="1000"
                prop:value=move || position().to_string()
                on:change=move |ev| {
                    if let Ok(v) = event_target_value(&ev).parse::<u64>() {
                        do_seek(v);
                    }
                }
            />
            <div class="np-progress">
                <div
                    class="np-progress-bar"
                    style=move || format!("width: {:.1}%", progress_pct())
                />
            </div>
            <div class="np-controls">
                <button class="np-btn" on:click=move |_| do_previous()>"⏮"</button>
                <button class="np-btn np-btn-big" on:click=move |_| do_pause()>"⏸"</button>
                <button class="np-btn np-btn-big" on:click=move |_| do_play()>"▶"</button>
                <button class="np-btn" on:click=move |_| do_skip()>"⏭"</button>
            </div>
        </div>
    }
}

fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}
