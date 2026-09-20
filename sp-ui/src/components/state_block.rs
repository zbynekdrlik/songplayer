//! #194 ROUND 3b: ONE loading / empty / error renderer.
//!
//! Before #194 each page hand-rolled its own loading / empty / error markup with
//! its own wording (English "No lyrics loaded" next to Slovak "Žiadne
//! playlisty"). `StateBlock` is the ONE renderer with ONE Slovak wording set and
//! stable testids (`state-loading` / `state-empty` / `state-error`), so every
//! page's non-data states look the same.

use leptos::prelude::*;

/// Which non-data state to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateKind {
    /// Data is still loading.
    Loading,
    /// Loaded, but there is nothing to show.
    Empty,
    /// A fetch / action failed; carries the error detail.
    Error(String),
}

#[component]
pub fn StateBlock(
    kind: StateKind,
    /// Override the default empty wording (`Nič tu nie je`) — e.g. a page-
    /// specific "Žiadne playlisty". Ignored for the loading / error kinds.
    #[prop(optional)]
    empty_label: Option<String>,
) -> impl IntoView {
    match kind {
        StateKind::Loading => view! {
            <div class="state-block state-loading" data-testid="state-loading">"Načítavam…"</div>
        }
        .into_any(),
        StateKind::Empty => {
            let label = empty_label.unwrap_or_else(|| "Nič tu nie je".to_string());
            view! {
                <div class="state-block state-empty" data-testid="state-empty">{label}</div>
            }
            .into_any()
        }
        StateKind::Error(msg) => view! {
            <div class="state-block state-error" data-testid="state-error">
                {format!("Chyba: {msg}")}
            </div>
        }
        .into_any(),
    }
}
