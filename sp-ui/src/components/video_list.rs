//! Table of cached videos for a playlist, with a per-row "play this song
//! now" button (#134) — reuses the generic `POST /playlists/{id}/play-video`
//! endpoint that `api::post_live_play_video` already calls for the /live
//! setlist; nothing about that URL or payload is live-specific.
//!
//! Each row also carries an inline metadata editor (#136 T1): operators
//! correct the wall song/artist for rows the metadata pipeline wrote wrong
//! (Gemini-failed regex-fallback swaps, mojibake) via
//! `PATCH /api/v1/videos/{id}` — the deterministic lever chosen over R4
//! automatic order detection.

use leptos::prelude::*;
use sp_core::models::Video;

use crate::api;
use crate::components::video_list_stems::{StemsMarker, passes_stems_filter};

#[component]
pub fn VideoList(playlist_id: i64) -> impl IntoView {
    let videos = RwSignal::new(Vec::<Video>::new());
    let error_msg = RwSignal::new(String::new());
    // #177: "len so stemami" filter — keep only stems-ready songs.
    let only_stems = RwSignal::new(false);

    // Which row (video id) is currently being edited, and the working
    // song/artist buffers for that row. `None` == no row in edit mode.
    let editing_id = RwSignal::new(None::<i64>);
    let edit_song = RwSignal::new(String::new());
    let edit_artist = RwSignal::new(String::new());

    let pid = playlist_id;
    let load = move || {
        leptos::task::spawn_local(async move {
            let path = format!("/api/v1/playlists/{pid}/videos");
            if let Ok(v) = api::get::<Vec<Video>>(&path).await {
                videos.set(v);
            }
        });
    };
    let _load = Effect::new(move |_| load());

    view! {
        <div class="video-list">
            {move || {
                let err = error_msg.get();
                if err.is_empty() {
                    view! { <span></span> }.into_any()
                } else {
                    view! { <div class="video-list-error">{err}</div> }.into_any()
                }
            }}
            <label class="video-list-stems-filter">
                <input
                    type="checkbox"
                    data-testid="video-list-stems-filter"
                    prop:checked=move || only_stems.get()
                    on:change=move |ev| only_stems.set(event_target_checked(&ev))
                />
                "len so stemami"
            </label>
            <table>
                <thead>
                    <tr>
                        <th class="video-list-col-play"></th>
                        <th>"Song"</th>
                        <th>"Artist"</th>
                        <th>"Cached"</th>
                        <th>"Normalized"</th>
                        <th class="video-list-col-stems">"Stemy"</th>
                        <th class="video-list-col-edit"></th>
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=move || {
                            let os = only_stems.get();
                            videos
                                .get()
                                .into_iter()
                                .filter(|v| passes_stems_filter(os, v))
                                .collect::<Vec<_>>()
                        }
                        // Key on the fields this row RENDERS statically (id +
                        // the editable song/artist), not on id alone. `For`
                        // never re-runs `children` for an existing key, so an
                        // id-only key left the row showing the pre-edit
                        // song/artist after the save-triggered `load()`
                        // refresh (the child captured them by value). Folding
                        // song+artist into the key recreates just the corrected
                        // row when its stored metadata changes (#136 T1).
                        // Safe because `videos` is only ever REPLACED by
                        // `load()` (mount + post-save, after `editing_id` is
                        // cleared), never mutated mid-edit: if live/WebSocket
                        // refresh is ever added here, revisit — a key change
                        // while a row is being typed into would drop focus.
                        key=|v| (v.id, v.song.clone(), v.artist.clone())
                        children=move |video| {
                            let video_id = video.id;
                            let normalized = video.normalized;
                            let title = video.song.clone().unwrap_or_else(|| video.title.clone());
                            let orig_song = video.song.clone().unwrap_or_default();
                            let orig_artist = video.artist.clone().unwrap_or_default();
                            let artist_display = video.artist.clone().unwrap_or_default();
                            let last_error = (!normalized)
                                .then(|| video.last_download_error.clone())
                                .flatten();
                            let is_editing = move || editing_id.get() == Some(video_id);
                            view! {
                                <tr data-video-id=video_id.to_string()>
                                    <td class="video-list-col-play">
                                        <button
                                            class="video-list-btn-play"
                                            title=if normalized {
                                                "Play this song now"
                                            } else {
                                                "Not yet processed — cannot play"
                                            }
                                            disabled=!normalized
                                            data-testid="video-list-play"
                                            on:click=move |_| {
                                                leptos::task::spawn_local(async move {
                                                    if let Err(e) = api::post_live_play_video(
                                                        pid, video_id, None,
                                                    )
                                                    .await
                                                    {
                                                        error_msg.set(e);
                                                    } else {
                                                        error_msg.set(String::new());
                                                    }
                                                });
                                            }
                                        >
                                            "▶"
                                        </button>
                                    </td>
                                    <td>
                                        {
                                            let title = title.clone();
                                            let last_error = last_error.clone();
                                            move || {
                                                if is_editing() {
                                                    view! {
                                                        <input
                                                            class="video-list-edit-song"
                                                            data-testid="video-list-edit-song"
                                                            prop:value=move || edit_song.get()
                                                            on:input=move |ev| {
                                                                edit_song.set(event_target_value(&ev))
                                                            }
                                                        />
                                                    }
                                                        .into_any()
                                                } else {
                                                    let title = title.clone();
                                                    let last_error = last_error.clone();
                                                    view! {
                                                        <span>{title}</span>
                                                        {last_error
                                                            .map(|err| {
                                                                view! {
                                                                    <span
                                                                        class="video-list-error-icon"
                                                                        title=err
                                                                    >
                                                                        " ⚠"
                                                                    </span>
                                                                }
                                                            })}
                                                    }
                                                        .into_any()
                                                }
                                            }
                                        }
                                    </td>
                                    <td>
                                        {
                                            let artist_display = artist_display.clone();
                                            move || {
                                                if is_editing() {
                                                    view! {
                                                        <input
                                                            class="video-list-edit-artist"
                                                            data-testid="video-list-edit-artist"
                                                            prop:value=move || edit_artist.get()
                                                            on:input=move |ev| {
                                                                edit_artist.set(event_target_value(&ev))
                                                            }
                                                        />
                                                    }
                                                        .into_any()
                                                } else {
                                                    view! { <span>{artist_display.clone()}</span> }
                                                        .into_any()
                                                }
                                            }
                                        }
                                    </td>
                                    <td>{if video.cached { "Yes" } else { "No" }}</td>
                                    <td>{if video.normalized { "Yes" } else { "No" }}</td>
                                    <td class="video-list-col-stems">
                                        <StemsMarker state=video.stems_state.clone() />
                                    </td>
                                    <td class="video-list-col-edit">
                                        {
                                            let orig_song = orig_song.clone();
                                            let orig_artist = orig_artist.clone();
                                            move || {
                                                if is_editing() {
                                                    view! {
                                                        <button
                                                            class="video-list-btn-save"
                                                            data-testid="video-list-edit-save"
                                                            title="Save"
                                                            on:click=move |_| {
                                                                let song = edit_song.get_untracked();
                                                                let artist = edit_artist
                                                                    .get_untracked();
                                                                leptos::task::spawn_local(async move {
                                                                    match api::patch_video_metadata(
                                                                            video_id,
                                                                            &song,
                                                                            &artist,
                                                                        )
                                                                        .await
                                                                    {
                                                                        Ok(()) => {
                                                                            editing_id.set(None);
                                                                            error_msg.set(String::new());
                                                                            load();
                                                                        }
                                                                        Err(e) => error_msg.set(e),
                                                                    }
                                                                });
                                                            }
                                                        >
                                                            "Save"
                                                        </button>
                                                        <button
                                                            class="video-list-btn-cancel"
                                                            data-testid="video-list-edit-cancel"
                                                            title="Cancel"
                                                            on:click=move |_| {
                                                                editing_id.set(None);
                                                            }
                                                        >
                                                            "Cancel"
                                                        </button>
                                                    }
                                                        .into_any()
                                                } else {
                                                    let orig_song = orig_song.clone();
                                                    let orig_artist = orig_artist.clone();
                                                    view! {
                                                        <button
                                                            class="video-list-btn-edit"
                                                            data-testid="video-list-edit"
                                                            title="Edit song / artist"
                                                            on:click=move |_| {
                                                                edit_song.set(orig_song.clone());
                                                                edit_artist.set(orig_artist.clone());
                                                                error_msg.set(String::new());
                                                                editing_id.set(Some(video_id));
                                                            }
                                                        >
                                                            "✎"
                                                        </button>
                                                    }
                                                        .into_any()
                                                }
                                            }
                                        }
                                    </td>
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
        </div>
    }
}
