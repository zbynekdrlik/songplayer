//! #209 (B1 of EPIC #174): the dashboard "Program" control.
//!
//! SongPlayer is the master switcher: its own NDI output `SP-program` carries
//! whichever playlist output is cut to it. This control shows the on-program
//! source and cuts to another playlist on click (`POST /api/v1/program/cut`,
//! frame-accurate on the boundary after next, server-side). It polls
//! `GET /api/v1/program` once a second through the ONE shared `poll_into`, so a
//! cut made elsewhere (the API, Companion) shows up here too.
//!
//! #212: while the NDI input "OBS manuál" is enabled (`input.enabled` on
//! `GET /api/v1/program`) it is listed after the playlists as one more source,
//! cut with `{"source": -1}` (`PROGRAM_INPUT_ID`).
//!
//! Testids (set here, never by a caller): `program-control`, `program-source`
//! (the "Na programe: …" line), `program-cut` (one button per source, with
//! `data-playlist-id` (`-1` for the input) + `aria-pressed` on the on-program
//! one), `program-error`.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use sp_core::config::{PROGRAM_INPUT_ID, PROGRAM_INPUT_LABEL};

use crate::components::selection;
use crate::store::{DashboardStore, poll_into};

/// The part of `GET /api/v1/program` this control renders.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ProgramState {
    /// The on-program playlist id; `None` = nothing selected yet (the program
    /// carries its standby black).
    #[serde(default)]
    pub source: Option<i64>,
    /// #212: the NDI input's state.
    #[serde(default)]
    pub input: ProgramInput,
}

/// The part of `GET /api/v1/program` → `input` this control renders.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ProgramInput {
    /// The input is enabled in Nastavenia: it is a program source.
    #[serde(default)]
    pub enabled: bool,
}

#[component]
pub fn ProgramControl() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");
    let program = RwSignal::new(ProgramState::default());
    let error = RwSignal::new(None::<String>);

    let cancelled = RwSignal::new(false);
    on_cleanup(move || cancelled.set(true));
    let _poll = Effect::new(move |_| {
        poll_into("/api/v1/program", 1_000, cancelled, program);
    });

    // A Memo, so a poll that returns the same source never re-renders a button.
    let on_program = Memo::new(move |_| program.get().source);
    let input_enabled = Memo::new(move |_| program.get().input.enabled);
    let on_program_name = move || match on_program.get() {
        Some(PROGRAM_INPUT_ID) => PROGRAM_INPUT_LABEL.to_string(),
        Some(id) => store
            .playlists
            .get()
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| format!("#{id}")),
        None => "nič".to_string(),
    };

    let cut = move |id: i64| {
        spawn_local(async move {
            let body = serde_json::json!({ "source": id });
            match crate::api::post_json::<_, ProgramState>("/api/v1/program/cut", &body).await {
                Ok(state) => {
                    let _ = program.try_set(state);
                    let _ = error.try_set(None);
                }
                Err(e) => {
                    let _ = error.try_set(Some(format!("Strih zlyhal: {e}")));
                }
            }
        });
    };

    view! {
        <section class="program-control" data-testid="program-control">
            <div class="program-head">
                <span class="program-title">"Program"</span>
                <span class="program-source" data-testid="program-source">
                    {move || format!("Na programe: {}", on_program_name())}
                </span>
            </div>
            <div class="program-buttons">
                <For
                    each=move || selection::ordered(&store.playlists.get())
                    key=|p| (p.id, p.name.clone())
                    children=move |p| {
                        let pid = p.id;
                        let name = p.name.clone();
                        let is_on = move || on_program.get() == Some(pid);
                        view! {
                            <button
                                class="program-cut"
                                class:program-cut-on=is_on
                                data-testid="program-cut"
                                data-playlist-id=pid.to_string()
                                aria-pressed=move || is_on().to_string()
                                title="Strih na program"
                                on:click=move |_| cut(pid)
                            >
                                {name}
                            </button>
                        }
                    }
                />
                <Show when=move || input_enabled.get()>
                    <button
                        class="program-cut"
                        class:program-cut-on=move || on_program.get() == Some(PROGRAM_INPUT_ID)
                        data-testid="program-cut"
                        data-playlist-id=PROGRAM_INPUT_ID.to_string()
                        aria-pressed=move || (on_program.get() == Some(PROGRAM_INPUT_ID)).to_string()
                        title="Strih na program — vstup NDI z OBS"
                        on:click=move |_| cut(PROGRAM_INPUT_ID)
                    >
                        {PROGRAM_INPUT_LABEL}
                    </button>
                </Show>
            </div>
            <div class="program-error" data-testid="program-error">
                {move || error.get().unwrap_or_default()}
            </div>
        </section>
    }
}
