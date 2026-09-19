//! Dashboard song list for a playlist, built from the shared `SongRow` +
//! `StatusChips` (#194): each row is title/artist, a primary "play this song
//! now" action (disabled until the file is normalized), the four status chips
//! (stiahnuté / stemy / text / dabing), and the actions slot (dub toggle + the
//! inline metadata editor, #136 T1). The text chip is joined in from the lyrics
//! catalog for this playlist so the Dashboard shows the same chip as Live/Lyrics
//! without a server change.

use leptos::prelude::*;
use sp_core::models::Video;
use sp_core::status_chip::{dub_chip, file_chip, stems_chip, text_chip};

use crate::api;
use crate::components::dub_toggle::DubToggle;
use crate::components::song_row::SongRow;
use crate::components::status_chips::ChipView;
use crate::components::video_list_stems::passes_stems_filter;

/// A video enriched with its lyrics text state (joined from the lyrics catalog).
#[derive(Clone, PartialEq)]
struct VideoRow {
    video: Video,
    has_lyrics: bool,
    is_reference: bool,
    is_stale: bool,
    lyrics_tip: String,
}

#[component]
pub fn VideoList(playlist_id: i64) -> impl IntoView {
    let videos = RwSignal::new(Vec::<VideoRow>::new());
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
            let videos_res =
                api::get::<Vec<Video>>(&format!("/api/v1/playlists/{pid}/videos")).await;
            let lyrics = api::get_lyrics_songs(Some(pid)).await.unwrap_or_default();
            let Ok(vs) = videos_res else {
                return;
            };
            let lyr_map: std::collections::HashMap<i64, (bool, bool, bool, String)> = lyrics
                .into_iter()
                .filter_map(|s| {
                    let id = s["video_id"].as_i64()?;
                    let has = s["has_lyrics"].as_bool().unwrap_or(false);
                    let is_ref = s["lyrics_reference"].as_bool().unwrap_or(false);
                    let is_stale = s["is_stale"].as_bool().unwrap_or(false);
                    let source = s["source"].as_str().unwrap_or("—");
                    Some((id, (has, is_ref, is_stale, format!("zdroj: {source}"))))
                })
                .collect();
            let enriched = vs
                .into_iter()
                .map(|v| {
                    let (has_lyrics, is_reference, is_stale, lyrics_tip) =
                        lyr_map.get(&v.id).cloned().unwrap_or_default();
                    VideoRow {
                        video: v,
                        has_lyrics,
                        is_reference,
                        is_stale,
                        lyrics_tip,
                    }
                })
                .collect::<Vec<_>>();
            videos.set(enriched);
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
            <div class="song-list-head">
                <span class="song-list-head-name">"Skladba / Interpret"</span>
                <span class="song-list-head-status">"Stav"</span>
            </div>
            <div class="song-list">
                <For
                    each=move || {
                        let os = only_stems.get();
                        videos
                            .get()
                            .into_iter()
                            .filter(|r| passes_stems_filter(os, &r.video))
                            .collect::<Vec<_>>()
                    }
                    // Key on the fields this row RENDERS statically. `For` never
                    // re-runs `children` for an existing key, so the key must fold
                    // in the editable song/artist and the chip inputs so a
                    // post-save / post-fetch refresh recreates the changed row
                    // (#136 T1). Safe because `videos` is only ever REPLACED by
                    // `load()` (mount + post-save, after `editing_id` is cleared).
                    key=|r| {
                        (
                            r.video.id,
                            r.video.song.clone(),
                            r.video.artist.clone(),
                            r.video.cached,
                            r.video.normalized,
                            r.video.stems_state.clone(),
                            r.video.dub_status.clone(),
                            r.has_lyrics,
                            r.is_reference,
                            r.is_stale,
                        )
                    }
                    children=move |row| {
                        let video = row.video.clone();
                        let video_id = video.id;
                        let normalized = video.normalized;
                        let dub_requested = video.dub_requested;
                        let title = video.song.clone().unwrap_or_else(|| video.title.clone());
                        let artist = video.artist.clone().unwrap_or_default();
                        let orig_song = video.song.clone().unwrap_or_default();
                        let orig_artist = video.artist.clone().unwrap_or_default();
                        let last_error = (!normalized)
                            .then(|| video.last_download_error.clone())
                            .flatten();
                        let file = if let Some(err) = last_error.clone() {
                            ChipView::with_tip(file_chip(video.cached, normalized), err)
                        } else {
                            ChipView::new(file_chip(video.cached, normalized))
                        };
                        let chips = vec![
                            file,
                            ChipView::new(stems_chip(video.stems_state.as_deref(), None)),
                            ChipView::with_tip(
                                text_chip(row.has_lyrics, row.is_reference, row.is_stale),
                                row.lyrics_tip.clone(),
                            ),
                            ChipView::new(dub_chip(video.dub_status.as_deref())),
                        ];
                        let on_play = Callback::new(move |_| {
                            leptos::task::spawn_local(async move {
                                if let Err(e) = api::post_live_play_video(pid, video_id, None).await {
                                    error_msg.set(e);
                                } else {
                                    error_msg.set(String::new());
                                }
                            });
                        });
                        let is_editing = move || editing_id.get() == Some(video_id);
                        view! {
                            {move || {
                                if is_editing() {
                                    // Inline metadata editor (#136 T1).
                                    view! {
                                        <div
                                            class="song-row song-row-editing"
                                            data-video-id=video_id.to_string()
                                        >
                                            <input
                                                class="video-list-edit-song"
                                                data-testid="video-list-edit-song"
                                                prop:value=move || edit_song.get()
                                                on:input=move |ev| edit_song.set(event_target_value(&ev))
                                            />
                                            <input
                                                class="video-list-edit-artist"
                                                data-testid="video-list-edit-artist"
                                                prop:value=move || edit_artist.get()
                                                on:input=move |ev| {
                                                    edit_artist.set(event_target_value(&ev))
                                                }
                                            />
                                            <button
                                                type="button"
                                                class="song-row-btn video-list-btn-save"
                                                data-testid="video-list-edit-save"
                                                title="Uložiť"
                                                on:click=move |_| {
                                                    let song = edit_song.get_untracked();
                                                    let artist = edit_artist.get_untracked();
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
                                                "Uložiť"
                                            </button>
                                            <button
                                                type="button"
                                                class="song-row-btn video-list-btn-cancel"
                                                data-testid="video-list-edit-cancel"
                                                title="Zrušiť"
                                                on:click=move |_| editing_id.set(None)
                                            >
                                                "Zrušiť"
                                            </button>
                                        </div>
                                    }
                                        .into_any()
                                } else {
                                    let orig_song = orig_song.clone();
                                    let orig_artist = orig_artist.clone();
                                    view! {
                                        <SongRow
                                            video_id=video_id
                                            title=title.clone()
                                            artist=artist.clone()
                                            chips=chips.clone()
                                            on_play=on_play
                                            play_ready=normalized
                                        >
                                            <DubToggle video_id=video_id initial=dub_requested />
                                            <button
                                                type="button"
                                                class="song-row-btn video-list-btn-edit"
                                                data-testid="video-list-edit"
                                                title="Upraviť skladbu / interpreta"
                                                on:click=move |_| {
                                                    edit_song.set(orig_song.clone());
                                                    edit_artist.set(orig_artist.clone());
                                                    error_msg.set(String::new());
                                                    editing_id.set(Some(video_id));
                                                }
                                            >
                                                "✎"
                                            </button>
                                        </SongRow>
                                    }
                                        .into_any()
                                }
                            }}
                        }
                    }
                />
            </div>
        </div>
    }
}
