//! Primary control surface of `/live`: the current set list with per-row
//! actions and the global playback bar. "Pause" + "Play" on the global bar
//! remember the paused song+position client-side and resume via play-video
//! + seek — the server state machine treats `SceneOff` as Stop, so a plain
//! `/play` POST after `/pause` would select a fresh random song instead of
//! continuing. The pause/seek dance keeps the operator's one-tap resume
//! working without a backend state-machine rewrite.

use leptos::prelude::*;

use crate::api;
use crate::store::DashboardStore;

#[component]
pub fn LiveSetList(
    playlist_id: i64,
    #[prop(into)] refresh: Signal<u64>,
    on_changed: Callback<()>,
    store: DashboardStore,
) -> impl IntoView {
    let items = RwSignal::new(Vec::<serde_json::Value>::new());
    let songs = RwSignal::new(Vec::<serde_json::Value>::new());
    let error_msg = RwSignal::new(String::new());
    // `Some((video_id, position_ms))` after the operator pressed Pause;
    // cleared on Play (resume) or on any per-row play click.
    let paused_state = RwSignal::new(None::<(i64, u64)>);

    // Reload whenever `refresh` bumps (add/remove/initial mount).
    let _load = Effect::new(move |_| {
        let _tick = refresh.get();
        leptos::task::spawn_local(async move {
            let items_res = api::get_live_items(playlist_id).await;
            let songs_res = api::get_lyrics_songs(None).await;
            match (items_res, songs_res) {
                (Ok(i), Ok(s)) => {
                    items.set(i);
                    songs.set(s);
                }
                (Err(e), _) | (_, Err(e)) => error_msg.set(e),
            }
        });
    });

    // On mount default the ytlive playlist to "single" so the engine stops
    // after each song instead of auto-advancing — operators drive song
    // transitions manually during worship/training sets. They can still
    // flip the dropdown below to "continuous" or "loop" for a given song.
    let _default_mode = Effect::new(move |prev_run: Option<()>| {
        if prev_run.is_some() {
            return;
        }
        leptos::task::spawn_local(async move {
            let body = serde_json::json!({ "mode": "single" });
            let _ = api::put_json_empty(
                &format!("/api/v1/playback/{playlist_id}/mode"),
                &body,
            )
            .await;
        });
    });

    let enriched = move || {
        let idx: std::collections::HashMap<i64, serde_json::Value> = songs
            .get()
            .into_iter()
            .filter_map(|s| s["video_id"].as_i64().map(|id| (id, s)))
            .collect();
        items
            .get()
            .into_iter()
            .map(|it| {
                let video_id = it["video_id"].as_i64().unwrap_or(0);
                let meta = idx.get(&video_id).cloned().unwrap_or_default();
                (it, meta)
            })
            .collect::<Vec<_>>()
    };

    view! {
        <div class="live-setlist">
            <h2>"ytlive set list"</h2>
            <div class="live-setlist-error">{move || error_msg.get()}</div>
            <table class="live-setlist-table">
                // Actions cell first (after #) so the primary ▶ / ✕ buttons
                // stay pinned on the left edge of the screen and never overflow
                // on narrow phones. EN-off + reorder arrows move to a
                // secondary column that may scroll off on very narrow widths
                // — losing them is cheap, losing ▶ is not.
                <thead>
                    <tr>
                        <th>"#"</th>
                        <th class="live-setlist-col-play">""</th>
                        <th>"Song"</th>
                        <th class="live-setlist-col-secondary"></th>
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=enriched
                        key=|(it, _)| it["video_id"].as_i64().unwrap_or(0)
                        children=move |(item, meta)| {
                            let position = item["position"].as_i64().unwrap_or(0);
                            let video_id = item["video_id"].as_i64().unwrap_or(0);
                            let song = meta["song"].as_str().unwrap_or("—").to_string();
                            let song_for_confirm = song.clone();
                            let suppress_initial = meta["suppress_resolume_en"]
                                .as_bool()
                                .unwrap_or(false);
                            let spotify_track_id_initial = meta["spotify_track_id"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            view! {
                                <tr>
                                    <td>{position + 1}</td>
                                    <td class="live-setlist-col-play">
                                        <button
                                            class="live-setlist-btn live-setlist-btn-play"
                                            title="Play this song"
                                            on:click=move |_| {
                                                // Per-row play starts a specific song from zero —
                                                // clear any global pause state so the next global
                                                // Play click doesn't try to resume the *previous*
                                                // paused song instead.
                                                paused_state.set(None);
                                                leptos::task::spawn_local(async move {
                                                    if let Err(e) = api::post_live_play_video(
                                                        playlist_id, video_id,
                                                    ).await {
                                                        error_msg.set(e);
                                                    }
                                                });
                                            }
                                        >"▶"</button>
                                    </td>
                                    <td class="live-setlist-song">{song}</td>
                                    <td class="live-setlist-col-secondary live-setlist-secondary">
                                        <button
                                            class="live-setlist-btn live-setlist-btn-move"
                                            title="Move up"
                                            on:click=move |_| {
                                                leptos::task::spawn_local(async move {
                                                    match api::post_live_move_item(
                                                        playlist_id, video_id, "up",
                                                    ).await {
                                                        Ok(()) => on_changed.run(()),
                                                        Err(e) => error_msg.set(e),
                                                    }
                                                });
                                            }
                                        >"▲"</button>
                                        <button
                                            class="live-setlist-btn live-setlist-btn-move"
                                            title="Move down"
                                            on:click=move |_| {
                                                leptos::task::spawn_local(async move {
                                                    match api::post_live_move_item(
                                                        playlist_id, video_id, "down",
                                                    ).await {
                                                        Ok(()) => on_changed.run(()),
                                                        Err(e) => error_msg.set(e),
                                                    }
                                                });
                                            }
                                        >"▼"</button>
                                        <label
                                            class="live-setlist-enoff-inline"
                                            title="Suppress pushing the English lyric line to Resolume #sp-subs clips"
                                        >
                                            <input
                                                type="checkbox"
                                                prop:checked=suppress_initial
                                                on:change=move |ev| {
                                                    let checked = event_target_checked(&ev);
                                                    leptos::task::spawn_local(async move {
                                                        match api::patch_video_suppress_en(
                                                            video_id, checked,
                                                        ).await {
                                                            Ok(()) => on_changed.run(()),
                                                            Err(e) => error_msg.set(e),
                                                        }
                                                    });
                                                }
                                            />
                                            "EN"
                                        </label>
                                        <button
                                            class={
                                                let extra = if spotify_track_id_initial.is_empty() {
                                                    ""
                                                } else {
                                                    " has-spotify"
                                                };
                                                format!("live-setlist-btn live-setlist-btn-spotify{extra}")
                                            }
                                            title=if spotify_track_id_initial.is_empty() {
                                                "Paste Spotify track URL"
                                            } else {
                                                "Edit Spotify track URL"
                                            }
                                            on:click={
                                                let initial = spotify_track_id_initial.clone();
                                                move |_| {
                                                    let prompt_text = if initial.is_empty() {
                                                        String::new()
                                                    } else {
                                                        // Show the bare track ID (URL would not round-trip through
                                                        // server's parser if the operator doesn't edit it).
                                                        initial.clone()
                                                    };
                                                    let result = web_sys::window()
                                                        .and_then(|w| {
                                                            w.prompt_with_message_and_default(
                                                                "Paste Spotify track URL (or empty to clear)",
                                                                &prompt_text,
                                                            )
                                                            .ok()
                                                        })
                                                        .flatten();
                                                    if let Some(input) = result {
                                                        leptos::task::spawn_local(async move {
                                                            match api::patch_video_spotify_url(video_id, &input).await {
                                                                Ok(()) => on_changed.run(()),
                                                                Err(e) => error_msg.set(e),
                                                            }
                                                        });
                                                    }
                                                }
                                            }
                                        >"🎵"</button>
                                        <button
                                            class="live-setlist-btn live-setlist-btn-remove"
                                            title="Remove from set list"
                                            on:click=move |_| {
                                                // Confirm so a stray tap during a live set doesn't
                                                // silently drop a song the band still needs.
                                                let ok = web_sys::window()
                                                    .and_then(|w| w.confirm_with_message(
                                                        &format!("Remove \"{song_for_confirm}\" from the set list?"),
                                                    ).ok())
                                                    .unwrap_or(false);
                                                if !ok { return; }
                                                leptos::task::spawn_local(async move {
                                                    match api::delete_live_item(
                                                        playlist_id, video_id,
                                                    ).await {
                                                        Ok(()) => on_changed.run(()),
                                                        Err(e) => error_msg.set(e),
                                                    }
                                                });
                                            }
                                        >"✕"</button>
                                    </td>
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
            <div class="live-setlist-controls">
                <button
                    class="live-setlist-control-btn"
                    on:click=move |_| {
                        // If we have a paused snapshot, resume that same song
                        // at the recorded position. Otherwise fall through to
                        // the legacy /play endpoint (fresh selection).
                        let resume = paused_state.get();
                        paused_state.set(None);
                        leptos::task::spawn_local(async move {
                            if let Some((video_id, position_ms)) = resume {
                                if let Err(e) = api::post_live_play_video(
                                    playlist_id, video_id,
                                ).await {
                                    error_msg.set(e);
                                    return;
                                }
                                // Small delay so the pipeline has loaded the
                                // new song before the seek — seek is a no-op
                                // while the pipeline is idle.
                                gloo_timers::future::TimeoutFuture::new(300).await;
                                let _ = api::seek_playlist(playlist_id, position_ms).await;
                            } else {
                                let _ = api::post_empty(
                                    &format!("/api/v1/playback/{playlist_id}/play"),
                                ).await;
                            }
                        });
                    }
                >"▶ Play"</button>
                <button
                    class="live-setlist-control-btn"
                    on:click=move |_| {
                        // Snapshot current video + position from the store
                        // BEFORE we POST /pause — the server transitions to
                        // WaitingForScene on pause and the NowPlaying stream
                        // stops updating. Save first, pause after.
                        let snapshot = store.now_playing.with(|map| {
                            map.get(&playlist_id).map(|np| (np.video_id, np.position_ms))
                        });
                        paused_state.set(snapshot);
                        leptos::task::spawn_local(async move {
                            let _ = api::post_empty(
                                &format!("/api/v1/playback/{playlist_id}/pause"),
                            ).await;
                        });
                    }
                >"⏸"</button>
                <button
                    class="live-setlist-control-btn"
                    on:click=move |_| {
                        leptos::task::spawn_local(async move {
                            let _ = api::post_empty(
                                &format!("/api/v1/playback/{playlist_id}/skip"),
                            ).await;
                        });
                    }
                >"⏭"</button>
                <button
                    class="live-setlist-control-btn"
                    on:click=move |_| {
                        leptos::task::spawn_local(async move {
                            let _ = api::post_empty(
                                &format!("/api/v1/playback/{playlist_id}/previous"),
                            ).await;
                        });
                    }
                >"⏮"</button>
                // Playback mode: "single" stops the engine when the current
                // song ends (no auto-advance to the next set-list row) —
                // exactly what the operator wants during a training/worship
                // set where they drive song flow manually. "continuous"
                // auto-selects the next row. "loop" replays the current
                // song until the operator intervenes.
                <select
                    class="live-setlist-mode"
                    title="Playback mode (single = stop after current)"
                    on:change=move |ev| {
                        let val = event_target_value(&ev);
                        leptos::task::spawn_local(async move {
                            let body = serde_json::json!({ "mode": val });
                            let _ = api::put_json_empty(
                                &format!("/api/v1/playback/{playlist_id}/mode"),
                                &body,
                            ).await;
                        });
                    }
                >
                    <option value="single" selected=true>"Single (stop after)"</option>
                    <option value="continuous">"Continuous (auto-next)"</option>
                    <option value="loop">"Loop current"</option>
                </select>
            </div>
        </div>
    }
}
