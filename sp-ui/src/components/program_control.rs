//! #209 (B1 of EPIC #174): the dashboard "Program" control.
//!
//! SongPlayer is the master switcher: its own NDI output `SP-program` carries
//! whichever playlist output is cut to it. This control shows the on-program
//! source and cuts to another playlist on click (`POST /api/v1/program/cut`,
//! frame-accurate on the boundary after next, server-side). It polls
//! `GET /api/v1/program` once a second through the ONE shared `poll_into`, so a
//! cut made elsewhere (the API, Companion) shows up here too.
//!
//! #212: while the NDI input "OBS manuál" is a source (`input.enabled` with a
//! non-empty `input.source` on `GET /api/v1/program` — the server's own cut
//! rule) it is listed after the playlists, cut with `{"source": -1}`
//! (`PROGRAM_INPUT_ID`).
//!
//! #245: "Blank", SongPlayer's own black (`PROGRAM_BLANK_ID`), is always
//! listed last, cut with `{"source": -2}`: no playlist, no cg OBS, no NDI
//! input needed.
//!
//! #215: the "Prechod" line shows the transition every cut uses (a crossfade of
//! N ms or a hard cut, and where it comes from: the Nastavenia choice, or the
//! default fade when none is chosen — #221 L5 deleted cg OBS's transition as a
//! source), and the progress of a running fade (`transition` on
//! `GET /api/v1/program`).
//!
//! #221 ROZHODNUTÉ 6022247729: every consumer takes `SP-program`, so the
//! server refuses (409) a cut to a playlist that is inactive or whose scene
//! catalog names no scene — it would black them all. `GET /api/v1/program`
//! lists those playlists (`cut_refused`, the server's own rule, polled with
//! the rest), and their buttons are DISABLED with a tooltip saying why
//! (`sp_core::program_refusal::cut_button_title`, the vocabulary the server
//! records). Before the first poll nothing is disabled; a cut the server
//! refuses then (409 + `{reason, error}`) shows "Strih odmietnutý: <why>"
//! on the error line, `<why>` = the reason code's Slovak text.
//!
//! Testids (set here, never by a caller): `program-control`, `program-source`
//! (the "Na programe: …" line), `program-transition` (the "Prechod: …" line),
//! `program-cut` (one button per source, with `data-playlist-id` (`-1` for the
//! input, `-2` for Blank) + `aria-pressed` on the on-program one, `disabled` + the reason's
//! `title` on a refused playlist), `program-error`.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use sp_core::config::{
    PROGRAM_BLANK_ID, PROGRAM_BLANK_LABEL, PROGRAM_INPUT_ID, PROGRAM_INPUT_LABEL,
};
use sp_core::program_refusal::{cut_button_title, refusal_text};

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
    /// #215: the transition the next cut uses + the running window.
    #[serde(default)]
    pub transition: ProgramTransition,
    /// #221: the playlists a cut refuses now, with the reason; `None` before
    /// the first poll and when the server could not read them.
    #[serde(default)]
    pub cut_refused: Option<Vec<CutRefused>>,
}

/// One entry of `GET /api/v1/program` → `cut_refused`; also the part of a
/// refused cut's 409 body (`{reason, error}`) this control reads.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct CutRefused {
    #[serde(default)]
    pub source: i64,
    /// `sp_core::program_refusal::PLAYLIST_INACTIVE` or `NO_SCENE`.
    #[serde(default)]
    pub reason: String,
}

impl ProgramState {
    /// Why the server refuses a cut to playlist `pid` now, `None` when it
    /// does not.
    pub fn refusal(&self, pid: i64) -> Option<&str> {
        self.cut_refused
            .as_deref()?
            .iter()
            .find(|r| r.source == pid)
            .map(|r| r.reason.as_str())
    }
}

/// The part of `GET /api/v1/program` → `transition` this control renders.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ProgramTransition {
    /// `fade` or `cut`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub duration_ms: u32,
    /// `setting` or `fallback`.
    #[serde(default)]
    pub source: String,
    /// The running (or next) fade window.
    #[serde(default)]
    pub active: Option<ProgramWindow>,
}

/// A running fade window (`transition.active`).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ProgramWindow {
    /// Served boundaries in percent of the window.
    #[serde(default)]
    pub progress: u32,
}

impl ProgramTransition {
    /// The "Prechod: …" line, e.g. `Prechod: prelínanie 300 ms (predvolené)`,
    /// `Prechod: strih (nastavenie)`, or with a running fade
    /// `… — prebieha 44 %`.
    pub fn label(&self) -> String {
        // Before the first poll lands there is no transition to name yet.
        if self.kind.is_empty() {
            return String::new();
        }
        let what = if self.kind == "fade" {
            format!("prelínanie {} ms", self.duration_ms)
        } else {
            "strih".to_string()
        };
        let from = match self.source.as_str() {
            "setting" => "nastavenie",
            _ => "predvolené",
        };
        match &self.active {
            Some(w) => format!("Prechod: {what} ({from}) — prebieha {} %", w.progress),
            None => format!("Prechod: {what} ({from})"),
        }
    }
}

/// The part of `GET /api/v1/program` → `input` this control renders.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ProgramInput {
    /// The input is enabled in Nastavenia.
    #[serde(default)]
    pub enabled: bool,
    /// The configured NDI source name (empty = nothing to receive).
    #[serde(default)]
    pub source: String,
}

impl ProgramInput {
    /// A program source: enabled with a source name (the server's cut rule).
    pub fn is_source(&self) -> bool {
        self.enabled && !self.source.trim().is_empty()
    }
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
    let input_enabled = Memo::new(move |_| program.get().input.is_source());
    // #215: a Memo, so an unchanged poll never touches the line.
    let transition_label = Memo::new(move |_| program.get().transition.label());
    let on_program_name = move || match on_program.get() {
        Some(PROGRAM_INPUT_ID) => PROGRAM_INPUT_LABEL.to_string(),
        Some(PROGRAM_BLANK_ID) => PROGRAM_BLANK_LABEL.to_string(),
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
            let path = "/api/v1/program/cut";
            match crate::api::post_json_status::<_, ProgramState>(path, &body).await {
                Ok(state) => {
                    let _ = program.try_set(state);
                    let _ = error.try_set(None);
                }
                // #221: refused (an inactive or scene-less playlist; its
                // button was not disabled yet). Say why: the 409 body's
                // reason code in Slovak (the generic text for a body it
                // cannot read).
                Err((409, answer)) => {
                    let reason = serde_json::from_str::<CutRefused>(&answer)
                        .map(|r| r.reason)
                        .unwrap_or_default();
                    let why = refusal_text(&reason);
                    let _ = error.try_set(Some(format!("Strih odmietnutý: {why}")));
                }
                Err(e) => {
                    let e = crate::api::post_error(path, e);
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
                <span class="program-transition" data-testid="program-transition">
                    {move || transition_label.get()}
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
                        // A Memo, so a poll that keeps the refusal never
                        // touches the button.
                        let refusal = Memo::new(move |_| {
                            program.with(|s| s.refusal(pid).map(str::to_string))
                        });
                        view! {
                            <button
                                class="program-cut"
                                class:program-cut-on=is_on
                                data-testid="program-cut"
                                data-playlist-id=pid.to_string()
                                aria-pressed=move || is_on().to_string()
                                prop:disabled=move || refusal.with(Option::is_some)
                                title=move || refusal.with(|r| cut_button_title(r.as_deref()))
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
                <button
                    class="program-cut program-cut-blank"
                    class:program-cut-on=move || on_program.get() == Some(PROGRAM_BLANK_ID)
                    data-testid="program-cut"
                    data-playlist-id=PROGRAM_BLANK_ID.to_string()
                    aria-pressed=move || (on_program.get() == Some(PROGRAM_BLANK_ID)).to_string()
                    title="Strih na program — čierna, bez zvuku"
                    on:click=move |_| cut(PROGRAM_BLANK_ID)
                >
                    {PROGRAM_BLANK_LABEL}
                </button>
            </div>
            <div class="program-error" data-testid="program-error">
                {move || error.get().unwrap_or_default()}
            </div>
        </section>
    }
}
