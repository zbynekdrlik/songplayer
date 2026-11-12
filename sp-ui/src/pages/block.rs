//! #265: `/blok` — block a song on this node from a phone in the room.
//!
//! A phone-sized Slovak page with no navigation of its own (the route only):
//! the song on program now with "Zablokovať v <uzol>", a search over the
//! node's songs by the title the wall shows, and the blocked list with
//! "Odblokovať". Every rule (the title, the search, the song on program, the
//! button's words) is `sp_core::song_block`'s; this file only wires them.

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use sp_core::models::{Playlist, Video};
use sp_core::player_view::program_badge;
use sp_core::song_block::{
    BlockRequest, BlockedSong, CatalogRow, ProgramCandidate, ProgramView, block_label, catalog,
    is_blocked, program_view, search, shown_title, wall_title,
};

use crate::api;
use crate::components::selection::states_known;
use crate::store::DashboardStore;

/// The part of `/api/v1/status` the page reads.
#[derive(Debug, Default, Clone, Deserialize)]
struct NodeInfo {
    #[serde(default)]
    node_name: Option<String>,
}

/// Every song row of the node by row id.
type Rows = HashMap<i64, CatalogRow>;

async fn load_rows(playlists: Vec<Playlist>) -> Rows {
    let mut rows = Rows::new();
    for p in playlists {
        let path = format!("/api/v1/playlists/{}/videos", p.id);
        if let Ok(videos) = api::get::<Vec<Video>>(&path).await {
            for v in videos {
                let song = v.song.as_deref().unwrap_or("").trim();
                let artist = v.artist.as_deref().unwrap_or("").trim();
                let has_metadata = !song.is_empty() || !artist.is_empty();
                let title = shown_title(v.song.as_deref(), v.artist.as_deref(), &v.title);
                rows.insert(
                    v.id,
                    CatalogRow {
                        row_id: v.id,
                        youtube_id: v.youtube_id,
                        title,
                        has_metadata,
                    },
                );
            }
        }
    }
    rows
}

#[component]
pub fn BlockPage() -> impl IntoView {
    let store = expect_context::<DashboardStore>();
    let node = RwSignal::new(None::<String>);
    // `None` until the list was read (#225: claim only what was told).
    let blocked = RwSignal::new(None::<Vec<BlockedSong>>);
    let rows = RwSignal::new(Rows::new());
    let query = RwSignal::new(String::new());
    let message = RwSignal::new(String::new());

    let reload_blocked = move || {
        spawn_local(async move {
            match api::get::<Vec<BlockedSong>>("/api/v1/blocked").await {
                Ok(list) => {
                    let _ = blocked.try_set(Some(list));
                }
                Err(e) => {
                    let _ = message.try_set(format!("Zoznam blokovaných sa nedá načítať: {e}"));
                }
            }
        });
    };

    Effect::new(move |_| {
        spawn_local(async move {
            if let Ok(info) = api::get::<NodeInfo>("/api/v1/status").await {
                let _ = node.try_set(info.node_name);
            }
        });
        reload_blocked();
    });

    // The node's songs, read again whenever the playlist list changes.
    Effect::new(move |_| {
        let playlists = store.playlists.get();
        spawn_local(async move {
            let loaded = load_rows(playlists).await;
            let _ = rows.try_set(loaded);
        });
    });

    let songs = Memo::new(move |_| rows.with(|r| catalog(r.values().cloned())));

    // The "Teraz na programe" area: loading, nothing, or the song on
    // program as (its YouTube id when the node's rows name it, its title).
    let current = Memo::new(move |_| {
        let known = store
            .playlists
            .with(|p| store.now_playing.with(|np| states_known(p, np)));
        store.now_playing.with(|np| {
            let candidates: Vec<ProgramCandidate> = np
                .iter()
                .map(|(pid, info)| ProgramCandidate {
                    playlist_id: *pid,
                    on_program: program_badge(info.state_known, info.state).is_on_program(),
                    video_id: info.has_now_playing_content().then_some(info.video_id),
                })
                .collect();
            match program_view(known, &candidates) {
                ProgramView::Loading => Err(true),
                ProgramView::Nothing => Err(false),
                ProgramView::Song {
                    playlist_id,
                    video_id,
                } => {
                    // The title the wall shows now (the song's own metadata,
                    // from THAT playlist's entry), else the row's.
                    let live = np
                        .get(&playlist_id)
                        .map(|i| wall_title(&i.song, &i.artist))
                        .unwrap_or_default();
                    let row = rows.with(|r| r.get(&video_id).cloned());
                    Ok(match row {
                        Some(r) if live.is_empty() => (Some(r.youtube_id), r.title),
                        Some(r) => (Some(r.youtube_id), live),
                        None => (None, live),
                    })
                }
            }
        })
    });

    let block = move |youtube_id: String| {
        spawn_local(async move {
            let req = BlockRequest { youtube_id };
            match api::post_json_status::<_, BlockedSong>("/api/v1/blocked", &req).await {
                Ok(song) => {
                    let _ = message.try_set(format!("Zablokované: {}", song.title));
                }
                Err((404, _)) => {
                    let _ = message.try_set("Táto pieseň na tomto uzle nie je.".to_string());
                }
                Err(e) => {
                    let _ = message.try_set(format!(
                        "Nepodarilo sa zablokovať: {}",
                        api::post_error("/api/v1/blocked", e)
                    ));
                }
            }
            reload_blocked();
        });
    };

    let unblock = move |youtube_id: String| {
        spawn_local(async move {
            let path = format!("/api/v1/blocked/{youtube_id}");
            match api::delete(&path).await {
                Ok(()) => {
                    let _ = message.try_set("Odblokované.".to_string());
                }
                Err(e) => {
                    let _ = message.try_set(format!("Nepodarilo sa odblokovať: {e}"));
                }
            }
            reload_blocked();
        });
    };

    let label = move || node.with(|n| block_label(n.as_deref()));

    view! {
        <div class="block-page" data-testid="block-page">
            <h1>"Blokovať pieseň"</h1>
            <p class="block-hint">
                "Zablokovaná pieseň sa na tomto uzle už sama nespustí. Operátor ju môže pustiť ručne."
            </p>

            <section
                class="block-section"
                data-testid="block-current"
                data-state=move || match current.get() {
                    Err(true) => "loading",
                    Err(false) => "nothing",
                    Ok(_) => "song",
                }
            >
                <h2>"Teraz na programe"</h2>
                {move || match current.get() {
                    Err(true) => view! {
                        <p class="block-current-title block-muted">"Načítavam…"</p>
                    }.into_any(),
                    Err(false) => view! {
                        <p class="block-current-title block-muted">"Na programe nehrá žiadna pieseň."</p>
                    }.into_any(),
                    Ok((youtube_id, title)) => {
                        let blocked_now = youtube_id.as_deref().is_some_and(|id| {
                            blocked.with(|b| b.as_deref().is_some_and(|b| is_blocked(b, id)))
                        });
                        let id = youtube_id.clone().unwrap_or_default();
                        view! {
                            <p class="block-current-title" data-testid="block-current-title">{title}</p>
                            <button
                                class="block-btn"
                                data-testid="block-current-btn"
                                prop:disabled=move || { youtube_id.is_none() || blocked_now }
                                on:click=move |_| block(id.clone())
                            >
                                {move || if blocked_now { "Už je zablokovaná".to_string() } else { label() }}
                            </button>
                        }.into_any()
                    }
                }}
            </section>

            <section class="block-section">
                <h2>"Hľadať pieseň"</h2>
                <input
                    type="search"
                    class="block-search"
                    data-testid="block-search"
                    autocomplete="off"
                    placeholder="Názov ako na stene…"
                    prop:value=move || query.get()
                    on:input=move |ev| query.set(event_target_value(&ev))
                />
                <ul class="block-results">
                    {move || {
                        let q = query.get();
                        songs.with(|all| {
                            search(all, &q)
                                .into_iter()
                                .map(|s| {
                                    let id = s.youtube_id.clone();
                                    let already = blocked.with(|b| {
                                        b.as_deref().is_some_and(|b| is_blocked(b, &id))
                                    });
                                    let attr = id.clone();
                                    view! {
                                        <li class="block-row" data-testid="block-result" data-youtube-id=attr>
                                            <span class="block-row-title">{s.title.clone()}</span>
                                            <button
                                                class="block-btn"
                                                prop:disabled=already
                                                on:click=move |_| block(id.clone())
                                            >
                                                {if already { "Zablokovaná".to_string() } else { "Zablokovať".to_string() }}
                                            </button>
                                        </li>
                                    }
                                })
                                .collect_view()
                        })
                    }}
                </ul>
            </section>

            <section class="block-section">
                <h2>"Zablokované piesne"</h2>
                <ul
                    class="block-list"
                    data-testid="block-list"
                    data-loaded=move || if blocked.with(Option::is_some) { "true" } else { "false" }
                >
                    {move || {
                        let Some(list) = blocked.get() else {
                            return view! { <li class="block-muted">"Načítavam…"</li> }.into_any();
                        };
                        if list.is_empty() {
                            return view! { <li class="block-muted">"Žiadna pieseň nie je zablokovaná."</li> }.into_any();
                        }
                        list.into_iter()
                            .map(|b| {
                                let id = b.youtube_id.clone();
                                view! {
                                    <li class="block-row" data-youtube-id=b.youtube_id.clone()>
                                        <span class="block-row-title">{b.title}</span>
                                        <button
                                            class="block-btn block-btn-undo"
                                            data-testid="block-unblock"
                                            on:click=move |_| unblock(id.clone())
                                        >
                                            "Odblokovať"
                                        </button>
                                    </li>
                                }
                            })
                            .collect_view()
                            .into_any()
                    }}
                </ul>
            </section>

            <p class="block-message" data-testid="block-message">{move || message.get()}</p>
        </div>
    }
}
